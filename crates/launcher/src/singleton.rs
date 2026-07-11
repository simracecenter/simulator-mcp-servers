//! Named-mutex singleton guard (ADR 0001 D3): stops a second launch from
//! starting a competing MCP server. Real enforcement is Windows-only, since
//! the launcher only ever ships for Windows; elsewhere this is a no-op so
//! `cargo check`/`cargo test` keep working in the Linux devcontainer.

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    use winapi::shared::winerror::ERROR_ALREADY_EXISTS;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::synchapi::CreateMutexW;
    use winapi::um::winnt::HANDLE;

    pub struct SingletonGuard {
        handle: HANDLE,
    }

    impl SingletonGuard {
        pub fn acquire(name: &str) -> Result<Self, String> {
            let wide: Vec<u16> = OsStr::new(name)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            // Safety: `wide` is a valid, NUL-terminated UTF-16 string that
            // outlives this call.
            let handle = unsafe { CreateMutexW(null_mut(), 0, wide.as_ptr()) };
            if handle.is_null() {
                return Err("CreateMutexW failed".to_string());
            }

            // Safety: `handle` was just created above by CreateMutexW.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe { CloseHandle(handle) };
                return Err(format!(
                    "another instance is already running (mutex \"{name}\")"
                ));
            }

            Ok(Self { handle })
        }
    }

    impl Drop for SingletonGuard {
        fn drop(&mut self) {
            // Safety: `self.handle` was created by CreateMutexW in
            // `acquire` and is only ever closed once, here.
            unsafe { CloseHandle(self.handle) };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// Stand-in so the workspace type-checks off Windows.
    pub struct SingletonGuard;

    impl SingletonGuard {
        pub fn acquire(_name: &str) -> Result<Self, String> {
            Ok(Self)
        }
    }
}

pub use imp::SingletonGuard;
