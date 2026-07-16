//! v1 [`LauncherUi`](super::LauncherUi) implementation: a system tray icon +
//! minimal native window, using `native-windows-gui`. Crate choice recorded
//! on the project board and in ADR 0001 D2 (2026-07-10 update).

#[cfg(windows)]
mod imp {
    use native_windows_derive as nwd;
    use native_windows_gui as nwg;
    use nwd::NwgUi;
    use nwg::NativeUi;

    use crate::ui::LauncherUi;

    // The `NwgUi` derive macro generates a companion `TrayUiUi` wrapper (in a
    // private `tray_ui_ui` submodule) that implements `NativeUi` and derefs
    // to `TrayUi`; `build_ui()` returns that wrapper, not `TrayUi` itself.
    use self::tray_ui_ui::TrayUiUi;

    /// "Director Console" — Sim RaceCenter's name for the main control
    /// interface (never "Admin Panel"; see the brand & voice guide).
    #[derive(Default, NwgUi)]
    pub struct TrayUi {
        #[nwg_control(size: (320, 160), position: (300, 300), title: "Director Console")]
        #[nwg_events(OnWindowClose: [nwg::stop_thread_dispatch()])]
        window: nwg::Window,

        #[nwg_control(tip: Some("Sim RaceCenter — Director Console"))]
        #[nwg_events(MousePressLeftUp: [TrayUi::show_window])]
        tray: nwg::TrayNotification,
    }

    impl TrayUi {
        fn show_window(&self) {
            self.window.set_visible(true);
        }
    }

    impl LauncherUi for TrayUi {
        fn run(&self) -> Result<(), String> {
            nwg::dispatch_thread_events();
            Ok(())
        }
    }

    pub fn build() -> Result<TrayUiUi, String> {
        nwg::init().map_err(|error| error.to_string())?;
        TrayUi::build_ui(Default::default()).map_err(|error| error.to_string())
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::ui::LauncherUi;

    /// Stand-in so the workspace type-checks off Windows; `native-windows-gui`
    /// only builds for Windows targets, matching ADR 0001 D2/§6.
    #[derive(Default)]
    pub struct TrayUi;

    impl LauncherUi for TrayUi {
        fn run(&self) -> Result<(), String> {
            Err("tray UI is only available on Windows builds".to_string())
        }
    }

    pub fn build() -> Result<TrayUi, String> {
        Ok(TrayUi)
    }
}

// `TrayUi` itself isn't named outside this module yet (only used via the
// `LauncherUi` trait object), but is kept public for tests/future callers.
#[allow(unused_imports)]
pub use imp::{build, TrayUi};

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use crate::ui::LauncherUi;

    #[test]
    fn non_windows_tray_builds_but_cannot_run() {
        let tray = build().unwrap();

        assert_eq!(
            tray.run().unwrap_err(),
            "tray UI is only available on Windows builds"
        );
    }
}
