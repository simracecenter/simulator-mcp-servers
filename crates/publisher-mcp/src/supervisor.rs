// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::secret::SecretString;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PublisherState {
    Stopped,
    Starting,
    Running,
    Failed,
    Unknown,
}

pub struct LaunchSpec {
    pub ingest_url: String,
    pub token: SecretString,
    pub cert_fingerprint: Option<String>,
    pub driver_display_name: Option<String>,
}

impl std::fmt::Debug for LaunchSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchSpec")
            .field("ingest_url", &self.ingest_url)
            .field("token", &self.token)
            .field("cert_fingerprint", &self.cert_fingerprint)
            .field("driver_display_name", &self.driver_display_name)
            .finish()
    }
}

impl Clone for LaunchSpec {
    fn clone(&self) -> Self {
        Self {
            ingest_url: self.ingest_url.clone(),
            token: self.token.clone(),
            cert_fingerprint: self.cert_fingerprint.clone(),
            driver_display_name: self.driver_display_name.clone(),
        }
    }
}

pub fn build_command(exe: &Path, spec: &LaunchSpec) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("--headless")
        .env("PUBLISHER_DESTINATION", "local")
        .env("PUBLISHER_LOCAL_URL", &spec.ingest_url)
        .env("PUBLISHER_LOCAL_TOKEN", spec.token.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(fingerprint) = &spec.cert_fingerprint {
        command.env("PUBLISHER_LOCAL_CERT_FINGERPRINT", fingerprint);
    }
    if let Some(name) = &spec.driver_display_name {
        command.env("PUBLISHER_DRIVER_DISPLAY_NAME", name);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x200);
    }
    command
}

pub trait ChildLauncher: Send + Sync {
    fn launch(&self, spec: &LaunchSpec) -> std::io::Result<Child>;
    fn exe_path(&self) -> &Path;
}

pub struct ExeLauncher {
    exe: PathBuf,
}

impl ExeLauncher {
    pub fn new(exe: PathBuf) -> Self {
        Self { exe }
    }
}

impl ChildLauncher for ExeLauncher {
    fn launch(&self, spec: &LaunchSpec) -> std::io::Result<Child> {
        build_command(&self.exe, spec).spawn()
    }

    fn exe_path(&self) -> &Path {
        &self.exe
    }
}

struct Inner {
    child: Option<Child>,
    started_at: Option<Instant>,
    last_exit_code: Option<i32>,
    stop_requested: bool,
    state_after_exit: PublisherState,
}

pub struct Supervisor {
    launcher: Box<dyn ChildLauncher>,
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorStatus {
    pub state: PublisherState,
    pub pid: Option<u32>,
    pub uptime_seconds: Option<u64>,
    pub last_exit_code: Option<i32>,
}

impl Supervisor {
    pub fn new(launcher: Box<dyn ChildLauncher>) -> Self {
        Self {
            launcher,
            inner: Mutex::new(Inner {
                child: None,
                started_at: None,
                last_exit_code: None,
                stop_requested: false,
                state_after_exit: PublisherState::Stopped,
            }),
        }
    }

    pub fn start(&self, spec: &LaunchSpec) -> std::io::Result<SupervisorStatus> {
        let mut inner = self.inner.lock().expect("supervisor lock is poisoned");
        if inner.child.is_some() {
            let status = status_locked(&mut inner);
            if matches!(
                status.state,
                PublisherState::Starting | PublisherState::Running
            ) {
                return Ok(status);
            }
        }
        let child = self.launcher.launch(spec)?;
        inner.child = Some(child);
        inner.started_at = Some(Instant::now());
        inner.last_exit_code = None;
        inner.stop_requested = false;
        inner.state_after_exit = PublisherState::Failed;
        Ok(status_locked(&mut inner))
    }

    pub fn stop(&self) -> SupervisorStatus {
        let mut inner = self.inner.lock().expect("supervisor lock is poisoned");
        if inner.child.is_none() {
            return status_locked(&mut inner);
        }

        inner.stop_requested = true;
        if let Some(child) = inner.child.as_ref() {
            let pid = child.id();
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status();
            }
            #[cfg(windows)]
            unsafe {
                use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
                let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            }
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let child_status = inner.child.as_mut().map(|child| child.try_wait());
            let exited = match child_status {
                Some(Ok(Some(status))) => {
                    inner.last_exit_code = status.code();
                    true
                }
                Some(Ok(None)) => false,
                Some(Err(_)) => {
                    inner.state_after_exit = PublisherState::Unknown;
                    true
                }
                None => true,
            };
            if exited || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        if let Some(mut child) = inner.child.take() {
            if let Ok(None) = child.try_wait() {
                let _ = child.kill();
            }
            if let Ok(status) = child.wait() {
                inner.last_exit_code = status.code();
            }
        }
        inner.state_after_exit = PublisherState::Stopped;
        status_locked(&mut inner)
    }

    pub fn status(&self) -> SupervisorStatus {
        let mut inner = self.inner.lock().expect("supervisor lock is poisoned");
        status_locked(&mut inner)
    }

    pub fn exe_path(&self) -> &Path {
        self.launcher.exe_path()
    }
}

fn status_locked(inner: &mut Inner) -> SupervisorStatus {
    let mut state = inner.state_after_exit;
    let mut pid = None;
    let mut uptime_seconds = None;
    let child_status = inner.child.as_mut().map(|child| {
        let pid = child.id();
        (pid, child.try_wait())
    });
    if let Some((child_pid, status)) = child_status {
        match status {
            Ok(Some(status)) => {
                inner.last_exit_code = status.code();
                inner.child = None;
                state = if inner.stop_requested {
                    PublisherState::Stopped
                } else {
                    PublisherState::Failed
                };
            }
            Ok(None) => {
                pid = Some(child_pid);
                let elapsed = inner
                    .started_at
                    .map(|started| started.elapsed().as_secs())
                    .unwrap_or_default();
                uptime_seconds = Some(elapsed);
                state = if elapsed < 1 {
                    PublisherState::Starting
                } else {
                    PublisherState::Running
                };
            }
            Err(_) => state = PublisherState::Unknown,
        }
    }

    SupervisorStatus {
        state,
        pid,
        uptime_seconds,
        last_exit_code: inner.last_exit_code,
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Ok(inner) = self.inner.get_mut() {
            if let Some(mut child) = inner.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeLauncher {
        exe: PathBuf,
        launches: Arc<AtomicUsize>,
        crash: bool,
    }

    impl FakeLauncher {
        fn new(crash: bool) -> Self {
            Self {
                exe: PathBuf::from("fake-publisher"),
                launches: Arc::new(AtomicUsize::new(0)),
                crash,
            }
        }
    }

    impl ChildLauncher for FakeLauncher {
        fn launch(&self, _spec: &LaunchSpec) -> std::io::Result<Child> {
            self.launches.fetch_add(1, Ordering::SeqCst);
            #[cfg(unix)]
            {
                if self.crash {
                    Command::new("sh").args(["-c", "exit 3"]).spawn()
                } else {
                    Command::new("sh").args(["-c", "sleep 30"]).spawn()
                }
            }
            #[cfg(windows)]
            {
                if self.crash {
                    Command::new("cmd").args(["/c", "exit 3"]).spawn()
                } else {
                    Command::new("cmd")
                        .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
                        .spawn()
                }
            }
        }

        fn exe_path(&self) -> &Path {
            &self.exe
        }
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            ingest_url: "https://director.example.com".to_string(),
            token: SecretString::new("secret"),
            cert_fingerprint: None,
            driver_display_name: None,
        }
    }

    #[test]
    fn start_reports_a_live_process_and_idempotently_reuses_it() {
        let launcher = FakeLauncher::new(false);
        let launches = Arc::clone(&launcher.launches);
        let supervisor = Supervisor::new(Box::new(launcher));
        let first = supervisor.start(&spec()).unwrap();
        assert!(matches!(
            first.state,
            PublisherState::Starting | PublisherState::Running
        ));
        assert!(first.pid.is_some());
        std::thread::sleep(Duration::from_millis(1100));
        let running = supervisor.status();
        assert_eq!(running.state, PublisherState::Running);
        let second = supervisor.start(&spec()).unwrap();
        assert_eq!(second.pid, running.pid);
        assert_eq!(launches.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stop_reports_stopped_without_a_pid() {
        let supervisor = Supervisor::new(Box::new(FakeLauncher::new(false)));
        supervisor.start(&spec()).unwrap();
        let status = supervisor.stop();
        assert_eq!(status.state, PublisherState::Stopped);
        assert_eq!(status.pid, None);
    }

    #[test]
    fn exited_process_reports_failed_and_exit_code() {
        let supervisor = Supervisor::new(Box::new(FakeLauncher::new(true)));
        supervisor.start(&spec()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let status = supervisor.status();
        assert_eq!(status.state, PublisherState::Failed);
        assert_eq!(status.last_exit_code, Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_supervisor_kills_a_live_process() {
        let supervisor = Supervisor::new(Box::new(FakeLauncher::new(false)));
        let pid = supervisor.start(&spec()).unwrap().pid.unwrap();
        drop(supervisor);
        let result = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap();
        assert!(!result.success());
    }

    #[test]
    fn build_command_keeps_token_out_of_argv_and_places_it_in_environment() {
        let command = build_command(
            Path::new("publisher.exe"),
            &LaunchSpec {
                ingest_url: "https://director.example.com".to_string(),
                token: SecretString::new("secret"),
                cert_fingerprint: Some("abc".to_string()),
                driver_display_name: Some("Driver".to_string()),
            },
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("--headless")]);
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "PUBLISHER_LOCAL_TOKEN"),
            Some((
                std::ffi::OsStr::new("PUBLISHER_LOCAL_TOKEN"),
                Some(std::ffi::OsStr::new("secret"))
            ))
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "PUBLISHER_DESTINATION"),
            Some((
                std::ffi::OsStr::new("PUBLISHER_DESTINATION"),
                Some(std::ffi::OsStr::new("local"))
            ))
        );
    }
}
