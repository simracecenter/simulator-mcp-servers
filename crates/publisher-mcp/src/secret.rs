// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::fmt;

pub trait TokenStore: Send + Sync {
    fn save(&self, token: &str) -> std::io::Result<()>;
    fn load(&self) -> std::io::Result<Option<String>>;
    fn clear(&self) -> std::io::Result<()>;
}

pub struct InMemoryTokenStore(Mutex<Option<String>>);

impl InMemoryTokenStore {
    pub fn new(token: Option<String>) -> Self {
        Self(Mutex::new(token))
    }
}

impl Default for InMemoryTokenStore {
    fn default() -> Self {
        Self::new(None)
    }
}

impl TokenStore for InMemoryTokenStore {
    fn save(&self, token: &str) -> std::io::Result<()> {
        *self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("token lock is poisoned"))? =
            Some(token.to_string());
        Ok(())
    }

    fn load(&self) -> std::io::Result<Option<String>> {
        self.0
            .lock()
            .map(|token| token.clone())
            .map_err(|_| std::io::Error::other("token lock is poisoned"))
    }

    fn clear(&self) -> std::io::Result<()> {
        *self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("token lock is poisoned"))? = None;
        Ok(())
    }
}

pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Clone for SecretString {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(***)")
    }
}

pub fn default_token_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("SimRaceCenter").join("publisher.token")
}

#[cfg(windows)]
pub struct DpapiTokenStore {
    pub path: PathBuf,
}

#[cfg(windows)]
impl DpapiTokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[cfg(windows)]
impl TokenStore for DpapiTokenStore {
    fn save(&self, token: &str) -> std::io::Result<()> {
        use std::ffi::c_void;
        use std::ptr;
        use winapi::shared::minwindef::DWORD;
        use winapi::um::dpapi::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN};
        use winapi::um::winbase::LocalFree;
        use winapi::um::wincrypt::DATA_BLOB;

        let mut input = DATA_BLOB {
            cbData: token.len() as DWORD,
            pbData: token.as_ptr() as *mut u8,
        };
        let mut output = DATA_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        let success = unsafe {
            CryptProtectData(
                &mut input,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if success == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let blob =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(output.pbData as *mut c_void);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, blob)
    }

    fn load(&self) -> std::io::Result<Option<String>> {
        use std::ffi::c_void;
        use std::ptr;
        use winapi::shared::minwindef::DWORD;
        use winapi::um::dpapi::CryptUnprotectData;
        use winapi::um::winbase::LocalFree;
        use winapi::um::wincrypt::DATA_BLOB;

        let blob = match std::fs::read(&self.path) {
            Ok(blob) => blob,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut input = DATA_BLOB {
            cbData: blob.len() as DWORD,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut output = DATA_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        let success = unsafe {
            CryptUnprotectData(
                &mut input,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                &mut output,
            )
        };
        if success == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let value = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
        let token = String::from_utf8(value.to_vec()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "token is not UTF-8")
        });
        unsafe {
            LocalFree(output.pbData as *mut c_void);
        }
        token.map(Some)
    }

    fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(not(windows))]
/// Non-Windows development/test fallback only; stores the token as plaintext.
pub struct InsecurePlaintextTokenStore {
    pub path: PathBuf,
}

#[cfg(not(windows))]
impl InsecurePlaintextTokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[cfg(not(windows))]
impl TokenStore for InsecurePlaintextTokenStore {
    fn save(&self, token: &str) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(token.as_bytes())
    }

    fn load(&self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(token) => Ok(Some(token)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

pub fn default_token_store() -> Arc<dyn TokenStore> {
    let path = default_token_path();
    #[cfg(windows)]
    {
        Arc::new(DpapiTokenStore::new(path))
    }
    #[cfg(not(windows))]
    {
        Arc::new(InsecurePlaintextTokenStore::new(path))
    }
}
