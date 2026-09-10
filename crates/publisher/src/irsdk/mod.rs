// SPDX-License-Identifier: GPL-3.0-or-later

//! iRacing shared-memory reader — Windows-only.
//!
//! Opens `Local\IRSDKMemMapFileName` and `Local\IRSDKDataValidEvent`,
//! parses the variable index once on connect, then provides
//! `wait_for_frame()` / `read_frame()` / `read_session_info()` for the
//! publisher main loop.
//!
//! The single `unsafe` boundary is in `SharedMemReader::try_connect()` where the
//! `MapViewOfFile` result is validated and wrapped into a `&[u8]` slice.
//! All downstream reads (`header.rs`, `reader.rs`) are safe slice operations.

pub mod header;
pub mod reader;

#[cfg(target_os = "windows")]
mod platform {
    use std::env;
    use std::ffi::OsStr;
    use std::iter;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    };
    use windows_sys::Win32::System::Threading::{OpenEventW, WaitForSingleObject};

    // SYNCHRONIZE is a standard Windows access-rights constant (not exported by windows-sys 0.59).
    const SYNCHRONIZE: u32 = 0x0010_0000;

    use crate::telemetry_frame::TelemetryFrame as CoreFrame;

    use super::header::{build_var_index, is_connected, latest_buf, parse_header, VarIndex};
    use super::reader::{build_frame, REQUIRED_VARS};

    const MMAP_NAME_DEFAULT: &str = "Local\\IRSDKMemMapFileName";
    const EVENT_NAME_DEFAULT: &str = "Local\\IRSDKDataValidEvent";
    const WAIT_TIMEOUT_MS: u32 = 1_000;
    const MAX_MMAP_SIZE: usize = 4 * 1024 * 1024;

    fn mmap_name() -> String {
        env::var("SIM_MMAP_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| MMAP_NAME_DEFAULT.to_owned())
    }

    fn event_name() -> String {
        env::var("SIM_EVENT_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| EVENT_NAME_DEFAULT.to_owned())
    }

    #[derive(Debug)]
    pub enum SharedMemError {
        /// iRacing is not running or the mmap is not present.
        NotRunning,
        /// The mmap data is malformed (header parse failed).
        BadHeader,
        /// One or more required telemetry variables are absent from the index.
        MissingVars,
        /// The Windows API returned an unexpected error.
        Os(u32),
    }

    impl std::fmt::Display for SharedMemError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::NotRunning => write!(f, "iRacing is not running"),
                Self::BadHeader => write!(f, "irsdk header parse failed"),
                Self::MissingVars => write!(f, "required telemetry variables missing from index"),
                Self::Os(code) => write!(f, "Windows API error {code:#010x}"),
            }
        }
    }

    /// Live iRacing shared-memory reader.
    ///
    /// Holds the Windows handles for the memory-mapped file and the
    /// data-valid event. `Drop` releases both handles automatically.
    pub struct SharedMemReader {
        map_view: *const u8,
        mmap_handle: HANDLE,
        event_handle: HANDLE,
        var_index: VarIndex,
        buf_len: usize,
    }

    // SAFETY: SharedMemReader owns its handles and the map_view pointer.
    // It is only ever moved into and used from the publisher main thread.
    unsafe impl Send for SharedMemReader {}

    impl SharedMemReader {
        /// Attempt to open the iRacing mmap and event handles.
        ///
        /// Returns `Err(SharedMemError::NotRunning)` if iRacing is not yet open;
        /// callers should retry after a delay.
        pub fn try_connect() -> Result<Self, SharedMemError> {
            let mmap_name = mmap_name();
            let mmap_handle =
                unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide(&mmap_name).as_ptr()) };
            if mmap_handle == std::ptr::null_mut() || mmap_handle == INVALID_HANDLE_VALUE {
                return Err(SharedMemError::NotRunning);
            }

            let map_view =
                unsafe { MapViewOfFile(mmap_handle, FILE_MAP_READ, 0, 0, 0).Value as *const u8 };
            if map_view.is_null() {
                unsafe { CloseHandle(mmap_handle) };
                return Err(SharedMemError::NotRunning);
            }

            let mmap_slice = unsafe { std::slice::from_raw_parts(map_view, MAX_MMAP_SIZE) };

            let hdr = parse_header(mmap_slice).ok_or(SharedMemError::BadHeader)?;

            if !is_connected(hdr.status) {
                unsafe {
                    UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: map_view as *mut _,
                    });
                    CloseHandle(mmap_handle);
                }
                return Err(SharedMemError::NotRunning);
            }

            let var_index = build_var_index(mmap_slice, &hdr, REQUIRED_VARS)
                .ok_or(SharedMemError::BadHeader)?;

            // Verify that non-optional required vars are present.
            let optional = [
                "LapLastLapTime",
                "SessionTick",
                "SessionState",
                "SessionNum",
                "CarIdxTrackSurface",
                "CarIdxLapCompleted",
            ];
            let missing = REQUIRED_VARS
                .iter()
                .filter(|&&n| !optional.contains(&n))
                .any(|name| !var_index.contains_key(*name));
            if missing {
                unsafe {
                    UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: map_view as *mut _,
                    });
                    CloseHandle(mmap_handle);
                }
                return Err(SharedMemError::MissingVars);
            }

            let event_name = event_name();
            let event_handle = unsafe { OpenEventW(SYNCHRONIZE, 0, wide(&event_name).as_ptr()) };
            if event_handle == std::ptr::null_mut() || event_handle == INVALID_HANDLE_VALUE {
                unsafe {
                    UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: map_view as *mut _,
                    });
                    CloseHandle(mmap_handle);
                }
                return Err(SharedMemError::NotRunning);
            }

            Ok(Self {
                map_view,
                mmap_handle,
                event_handle,
                var_index,
                buf_len: hdr.buf_len as usize,
            })
        }

        /// Block until iRacing signals a new 60 Hz frame or the 1 s timeout elapses.
        ///
        /// Returns:
        /// - `Ok(true)`  — new frame is ready
        /// - `Ok(false)` — timeout; iRacing may still be running
        /// - `Err`       — OS error
        pub fn wait_for_frame(&self) -> Result<bool, SharedMemError> {
            let result = unsafe { WaitForSingleObject(self.event_handle, WAIT_TIMEOUT_MS) };
            match result {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                other => Err(SharedMemError::Os(other)),
            }
        }

        /// Read the latest telemetry frame from the mmap.
        pub fn read_frame(&self) -> Option<CoreFrame> {
            let mmap = unsafe { std::slice::from_raw_parts(self.map_view, MAX_MMAP_SIZE) };
            let hdr = parse_header(mmap)?;
            let latest = latest_buf(&hdr.var_bufs);
            let start = latest.buf_offset as usize;
            let end = start + self.buf_len;

            if end > mmap.len() {
                return None;
            }

            build_frame(
                &mmap[start..end],
                &self.var_index,
                hdr.session_info_update as u32,
            )
        }

        /// Read the SessionInfo YAML blob from the mmap.
        ///
        /// The blob changes whenever `SessionInfoUpdate` increments.
        /// Returns `None` if the header is malformed or the UTF-8 decode fails.
        pub fn read_session_info(&self) -> Option<String> {
            let mmap = unsafe { std::slice::from_raw_parts(self.map_view, MAX_MMAP_SIZE) };
            let hdr = parse_header(mmap)?;
            let start = hdr.session_info_offset as usize;
            let len = hdr.session_info_len as usize;

            if start == 0 || len == 0 || start + len > mmap.len() {
                return None;
            }

            let bytes = &mmap[start..start + len];
            // The YAML string is null-terminated; trim to first null.
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(len);
            // iRacing payloads are expected to be UTF-8, but some sessions can
            // contain non-UTF8 bytes in free-text fields (driver/team names).
            // Use a lossy decode so YAML key scanning still succeeds.
            Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
        }

        /// `true` if the iRacing status field still shows a live session.
        pub fn is_connected(&self) -> bool {
            let mmap = unsafe { std::slice::from_raw_parts(self.map_view, MAX_MMAP_SIZE) };
            parse_header(mmap).map_or(false, |h| is_connected(h.status))
        }
    }

    impl Drop for SharedMemReader {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.map_view as *mut _,
                });
                CloseHandle(self.mmap_handle);
                CloseHandle(self.event_handle);
            }
        }
    }

    /// Encode a Rust `&str` as a null-terminated UTF-16 string for Windows APIs.
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(iter::once(0)).collect()
    }
}

#[cfg(target_os = "windows")]
pub use platform::{SharedMemError, SharedMemReader};
