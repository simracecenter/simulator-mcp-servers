// SPDX-License-Identifier: GPL-3.0-or-later

//! Wheel-button input backend — Windows Raw Input over HID.
//!
//! Fanatec and Asetek bases both present themselves as HID joysticks, so the
//! publisher listens to Raw Input for usage page `Generic Desktop` /
//! usages `Joystick` + `Gamepad` and reads the `Button` usage page from each
//! input report. No vendor SDK and no keyboard emulation is involved: the
//! driver binds whatever button their wheel actually reports.
//!
//! Raw Input is registered with `RIDEV_INPUTSINK` on a message-only window so
//! presses are seen while iRacing has the foreground.

use crate::controls::ButtonPress;

/// Extract the stable part of a Raw Input device path.
///
/// Paths look like `\\?\HID#VID_0EB7&PID_0E04&Col01#7&1f2b3c4d&0&0000#{guid}`.
/// The instance section changes when the wheel is re-plugged or moved to a
/// different USB port, so only the `VID_xxxx&PID_xxxx` pair is kept.
pub fn device_identity(device_path: &str) -> String {
    let upper = device_path.to_ascii_uppercase();
    let Some(vid_at) = upper.find("VID_") else {
        return device_path.to_owned();
    };
    let tail = &upper[vid_at..];
    let end = tail
        .find("&COL")
        .or_else(|| tail.find('#'))
        .unwrap_or(tail.len());
    tail[..end].to_owned()
}

/// Per-device button state, used to turn HID reports into press edges.
///
/// HID input reports carry the full button map on every report, so a held
/// button appears in every report. Only buttons that were absent from the
/// previous report for that device are reported as newly pressed.
#[derive(Debug, Default)]
pub struct PressTracker {
    devices: Vec<(String, Vec<u16>)>,
}

impl PressTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one report's pressed-button list; return the newly pressed buttons.
    pub fn update(&mut self, device: &str, pressed: &[u16]) -> Vec<u16> {
        let previous = match self.devices.iter().position(|(d, _)| d == device) {
            Some(i) => std::mem::replace(&mut self.devices[i].1, pressed.to_vec()),
            None => {
                self.devices.push((device.to_owned(), pressed.to_vec()));
                Vec::new()
            }
        };
        let mut down: Vec<u16> = pressed
            .iter()
            .copied()
            .filter(|b| !previous.contains(b))
            .collect();
        down.sort_unstable();
        down
    }

    pub fn device_count(&self) -> usize {
        self.devices.len()
    }
}

/// Build the press records for the buttons that just went down.
pub fn presses_for(device: &str, newly_down: &[u16]) -> Vec<ButtonPress> {
    newly_down
        .iter()
        .map(|b| ButtonPress {
            device: device.to_owned(),
            button: *b,
        })
        .collect()
}

// ── Windows Raw Input backend ─────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub use windows_backend::spawn;

#[cfg(target_os = "windows")]
mod windows_backend {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HidP_GetUsages, HidP_Input, HidP_MaxUsageListLength, HIDP_STATUS_SUCCESS,
    };
    use windows_sys::Win32::Foundation::{HANDLE, HWND};
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, RAWINPUT, RAWINPUTDEVICE,
        RAWINPUTHEADER, RIDEV_INPUTSINK, RIDI_DEVICENAME, RIDI_PREPARSEDDATA, RID_INPUT,
        RIM_TYPEHID,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, PeekMessageW, HWND_MESSAGE, MSG, PM_REMOVE, WM_INPUT,
    };

    use super::{device_identity, PressTracker};
    use crate::controls::{
        now_wall_clock_ms, CapturedBinding, ControlDispatcher, ControlRequest, ControlsState,
    };

    /// HID usage page for buttons.
    const USAGE_PAGE_BUTTON: u16 = 0x09;
    /// Generic Desktop usage page and the usages wheels present as.
    const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
    const USAGE_JOYSTICK: u16 = 0x04;
    const USAGE_GAMEPAD: u16 = 0x05;
    const USAGE_MULTI_AXIS: u16 = 0x08;

    /// Start the Raw Input listener thread.
    ///
    /// Accepted requests are sent on the returned channel; the pipeline thread
    /// drains it once per telemetry frame so every request is stamped with live
    /// session state. Errors are surfaced through `ControlsState::last_error`.
    pub fn spawn(
        controls: Arc<Mutex<ControlsState>>,
        running: Arc<AtomicBool>,
    ) -> Receiver<ControlRequest> {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("publisher-controls".into())
            .spawn(move || {
                if let Err(e) = watch(&controls, &running, &tx) {
                    eprintln!("[controls] input unavailable: {e}");
                    controls.lock().unwrap().last_error = Some(e);
                }
                controls.lock().unwrap().listening = false;
            });
        if let Err(e) = spawned {
            eprintln!("[controls] could not start input thread: {e}");
        }
        rx
    }

    fn watch(
        controls: &Arc<Mutex<ControlsState>>,
        running: &Arc<AtomicBool>,
        tx: &Sender<ControlRequest>,
    ) -> Result<(), String> {
        let hwnd = create_message_window()?;
        register_devices(hwnd)?;
        controls.lock().unwrap().listening = true;
        println!("[controls] listening for wheel buttons (Raw Input HID)");

        let mut tracker = PressTracker::new();
        let mut dispatcher = ControlDispatcher::new();
        let mut preparsed_cache: Vec<(isize, Vec<u8>)> = Vec::new();
        let mut name_cache: Vec<(isize, String)> = Vec::new();
        let started = Instant::now();

        while running.load(Ordering::SeqCst) {
            let mut msg = unsafe { std::mem::zeroed::<MSG>() };
            let got = unsafe { PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) } != 0;
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            if msg.message == WM_INPUT {
                let presses = read_presses(
                    msg.lParam,
                    &mut tracker,
                    &mut preparsed_cache,
                    &mut name_cache,
                );
                for press in presses {
                    let now_ms = started.elapsed().as_millis() as u64;
                    let mut state = controls.lock().unwrap();
                    state.devices_seen = tracker.device_count();
                    if let Some(action) = state.learning {
                        println!(
                            "[controls] bound {} to {} button {}",
                            action, press.device, press.button
                        );
                        state.apply_capture(CapturedBinding {
                            action,
                            device: press.device.clone(),
                            button: press.button,
                        });
                        continue;
                    }
                    let cfg = state.config.clone();
                    drop(state);

                    if let Some(request) =
                        dispatcher.on_button_down(&cfg, &press, now_ms, now_wall_clock_ms())
                    {
                        println!(
                            "[controls] {} requested ({} button {})",
                            request.action, request.device, request.button
                        );
                        {
                            let mut state = controls.lock().unwrap();
                            state.last_request = Some((request.action, request.requested_at_ms));
                        }
                        if tx.send(request).is_err() {
                            return Ok(()); // pipeline gone — stop listening
                        }
                    }
                }
            }
            // Raw Input requires the default window procedure to run so the
            // system can release the report buffer.
            unsafe { DispatchMessageW(&msg) };
        }
        Ok(())
    }

    fn create_message_window() -> Result<HWND, String> {
        // The predefined STATIC class avoids registering a window class just to
        // own a message-only window.
        let class = wide("STATIC");
        let title = wide("publisher-controls");
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err("CreateWindowExW failed".to_owned());
        }
        Ok(hwnd)
    }

    fn register_devices(hwnd: HWND) -> Result<(), String> {
        let devices: Vec<RAWINPUTDEVICE> = [USAGE_JOYSTICK, USAGE_GAMEPAD, USAGE_MULTI_AXIS]
            .into_iter()
            .map(|usage| RAWINPUTDEVICE {
                usUsagePage: USAGE_PAGE_GENERIC_DESKTOP,
                usUsage: usage,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            })
            .collect();
        let ok = unsafe {
            RegisterRawInputDevices(
                devices.as_ptr(),
                devices.len() as u32,
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            )
        };
        if ok == 0 {
            return Err("RegisterRawInputDevices failed".to_owned());
        }
        Ok(())
    }

    /// Decode one `WM_INPUT` message into the buttons that just went down.
    fn read_presses(
        lparam: isize,
        tracker: &mut PressTracker,
        preparsed_cache: &mut Vec<(isize, Vec<u8>)>,
        name_cache: &mut Vec<(isize, String)>,
    ) -> Vec<crate::controls::ButtonPress> {
        let mut size: u32 = 0;
        let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        unsafe {
            GetRawInputData(
                lparam as _,
                RID_INPUT,
                std::ptr::null_mut(),
                &mut size,
                header_size,
            )
        };
        if size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; size as usize];
        let read = unsafe {
            GetRawInputData(
                lparam as _,
                RID_INPUT,
                buf.as_mut_ptr() as *mut _,
                &mut size,
                header_size,
            )
        };
        if read == u32::MAX || (read as usize) < std::mem::size_of::<RAWINPUTHEADER>() {
            return Vec::new();
        }

        let raw = unsafe { &*(buf.as_ptr() as *const RAWINPUT) };
        if raw.header.dwType != RIM_TYPEHID {
            return Vec::new();
        }
        let handle = raw.header.hDevice;
        let hid = unsafe { raw.data.hid };
        let report_size = hid.dwSizeHid as usize;
        let report_count = hid.dwCount as usize;
        if report_size == 0 || report_count == 0 {
            return Vec::new();
        }

        let device = device_name(handle, name_cache);
        let preparsed = match preparsed_data(handle, preparsed_cache) {
            Some(p) => p,
            None => return Vec::new(),
        };

        // `bRawData` is a flexible array of `dwCount` reports of `dwSizeHid`.
        let reports_ptr = unsafe { std::ptr::addr_of!(raw.data.hid.bRawData) as *const u8 };
        let mut presses = Vec::new();
        for i in 0..report_count {
            let report = unsafe {
                std::slice::from_raw_parts(reports_ptr.add(i * report_size), report_size)
            };
            let pressed = pressed_buttons(preparsed, report);
            let down = tracker.update(&device, &pressed);
            presses.extend(super::presses_for(&device, &down));
        }
        presses
    }

    /// Pressed button usages in one HID input report.
    fn pressed_buttons(preparsed: &[u8], report: &[u8]) -> Vec<u16> {
        let preparsed_ptr = preparsed.as_ptr() as isize;
        let max = unsafe { HidP_MaxUsageListLength(HidP_Input, USAGE_PAGE_BUTTON, preparsed_ptr) };
        if max == 0 {
            return Vec::new();
        }
        let mut usages = vec![0u16; max as usize];
        let mut length = max;
        let status = unsafe {
            HidP_GetUsages(
                HidP_Input,
                USAGE_PAGE_BUTTON,
                0,
                usages.as_mut_ptr(),
                &mut length,
                preparsed_ptr,
                report.as_ptr() as *mut u8,
                report.len() as u32,
            )
        };
        if status != HIDP_STATUS_SUCCESS {
            return Vec::new();
        }
        usages.truncate(length as usize);
        usages
    }

    fn preparsed_data(handle: HANDLE, cache: &mut Vec<(isize, Vec<u8>)>) -> Option<&[u8]> {
        let key = handle as isize;
        if let Some(i) = cache.iter().position(|(k, _)| *k == key) {
            return Some(&cache[i].1);
        }
        let mut size: u32 = 0;
        unsafe {
            GetRawInputDeviceInfoW(handle, RIDI_PREPARSEDDATA, std::ptr::null_mut(), &mut size)
        };
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let read = unsafe {
            GetRawInputDeviceInfoW(
                handle,
                RIDI_PREPARSEDDATA,
                buf.as_mut_ptr() as *mut _,
                &mut size,
            )
        };
        if read == u32::MAX {
            return None;
        }
        cache.push((key, buf));
        cache.last().map(|(_, b)| b.as_slice())
    }

    /// Stable device identity for a Raw Input device handle.
    fn device_name(handle: HANDLE, cache: &mut Vec<(isize, String)>) -> String {
        let key = handle as isize;
        if let Some((_, name)) = cache.iter().find(|(k, _)| *k == key) {
            return name.clone();
        }
        let mut size: u32 = 0;
        unsafe { GetRawInputDeviceInfoW(handle, RIDI_DEVICENAME, std::ptr::null_mut(), &mut size) };
        let name = if size == 0 {
            format!("handle:{key}")
        } else {
            let mut buf = vec![0u16; size as usize];
            let read = unsafe {
                GetRawInputDeviceInfoW(
                    handle,
                    RIDI_DEVICENAME,
                    buf.as_mut_ptr() as *mut _,
                    &mut size,
                )
            };
            if read == u32::MAX {
                format!("handle:{key}")
            } else {
                let path = String::from_utf16_lossy(
                    &buf[..buf.iter().position(|c| *c == 0).unwrap_or(buf.len())],
                );
                device_identity(&path)
            }
        };
        cache.push((key, name.clone()));
        name
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_identity_keeps_vid_pid_only() {
        assert_eq!(
            device_identity(r"\\?\HID#VID_0EB7&PID_0E04&Col01#7&1f2b3c4d&0&0000#{4d1e55b2}"),
            "VID_0EB7&PID_0E04"
        );
        assert_eq!(
            device_identity(r"\\?\HID#VID_2433&PID_F300#8&2b3c4d5e&0&0000#{4d1e55b2}"),
            "VID_2433&PID_F300"
        );
    }

    #[test]
    fn device_identity_falls_back_to_full_path() {
        assert_eq!(device_identity(r"\\?\HID#WEIRD"), r"\\?\HID#WEIRD");
    }

    #[test]
    fn first_report_of_a_held_button_is_an_edge() {
        let mut t = PressTracker::new();
        assert_eq!(t.update("wheel", &[7]), vec![7]);
        assert_eq!(t.update("wheel", &[7]), Vec::<u16>::new());
        assert_eq!(t.update("wheel", &[]), Vec::<u16>::new());
        assert_eq!(t.update("wheel", &[7]), vec![7]);
    }

    #[test]
    fn simultaneous_buttons_are_both_edges_once() {
        let mut t = PressTracker::new();
        assert_eq!(t.update("wheel", &[3, 7]), vec![3, 7]);
        // Releasing one keeps the other held.
        assert_eq!(t.update("wheel", &[7]), Vec::<u16>::new());
        assert_eq!(t.update("wheel", &[3, 7]), vec![3]);
    }

    #[test]
    fn devices_are_tracked_independently() {
        let mut t = PressTracker::new();
        assert_eq!(t.update("fanatec", &[7]), vec![7]);
        assert_eq!(t.update("asetek", &[7]), vec![7]);
        assert_eq!(t.device_count(), 2);
    }

    #[test]
    fn presses_carry_device_and_button() {
        let presses = presses_for("VID_0EB7&PID_0E04", &[7, 9]);
        assert_eq!(presses.len(), 2);
        assert_eq!(presses[0].device, "VID_0EB7&PID_0E04");
        assert_eq!(presses[1].button, 9);
    }
}
