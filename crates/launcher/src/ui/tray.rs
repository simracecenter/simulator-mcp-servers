//! v1 [`LauncherUi`](super::LauncherUi) implementation: a system tray icon +
//! minimal native window, using `native-windows-gui`. Crate choice recorded
//! on the project board and in ADR 0001 D2 (2026-07-10 update).

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    use native_windows_derive as nwd;
    use native_windows_gui as nwg;
    use nwd::NwgUi;
    use nwg::NativeUi;

    use crate::ui::LauncherUi;

    // The `NwgUi` derive macro generates a companion `TrayUiUi` wrapper (in a
    // private `tray_ui_ui` submodule) that implements `NativeUi` and derefs
    // to `TrayUi`; `build_ui()` returns that wrapper, not `TrayUi` itself.
    use self::tray_ui_ui::TrayUiUi;

    static SETTINGS_URL: OnceLock<String> = OnceLock::new();

    /// "Director Console" — Sim RaceCenter's name for the main control
    /// interface (never "Admin Panel"; see the brand & voice guide).
    ///
    /// The tray icon is the resident launcher surface. Left-click opens the web
    /// Director Console; right-click shows a menu with Launch/Quit actions. No
    /// visible native window is shown.
    #[derive(Default, NwgUi)]
    pub struct TrayUi {
        #[nwg_resource(source_bin: Some(include_bytes!("../../assets/logo.ico")))]
        icon: nwg::Icon,

        #[nwg_control]
        message_window: nwg::MessageWindow,

        #[nwg_control(parent: message_window, icon: Some(&data.icon), tip: Some("Sim RaceCenter — Director Console"))]
        #[nwg_events(
            MousePressLeftUp: [TrayUi::open_web_ui],
            OnContextMenu: [TrayUi::show_menu]
        )]
        tray: nwg::TrayNotification,

        #[nwg_control(parent: message_window, popup: true)]
        tray_menu: nwg::Menu,

        #[nwg_control(parent: tray_menu, text: "Launch Director Console")]
        #[nwg_events(OnMenuItemSelected: [TrayUi::open_web_ui])]
        launch_item: nwg::MenuItem,

        #[nwg_control(parent: tray_menu, text: "Quit")]
        #[nwg_events(OnMenuItemSelected: [TrayUi::quit])]
        quit_item: nwg::MenuItem,
    }

    impl TrayUi {
        /// Show the tray context menu at the current cursor position.
        fn show_menu(&self) {
            let (x, y) = nwg::GlobalCursor::position();
            self.tray_menu.popup(x, y);
        }

        /// Open the Director Console in the user's default browser.
        fn open_web_ui(&self) {
            if let Some(url) = SETTINGS_URL.get() {
                if let Err(error) = open::that(url) {
                    let message = format!("Could not open Director Console: {error}");
                    self.tray.show(&message, Some("Sim RaceCenter"), None, None);
                }
            }
        }

        /// Stop the message loop so the launcher can shut down cleanly.
        fn quit(&self) {
            nwg::stop_thread_dispatch();
        }
    }

    impl LauncherUi for TrayUi {
        fn run(&self) -> Result<(), String> {
            nwg::dispatch_thread_events();
            Ok(())
        }
    }

    pub fn build(settings_url: String) -> Result<TrayUiUi, String> {
        let _ = SETTINGS_URL.set(settings_url);
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

    pub fn build(_settings_url: String) -> Result<TrayUi, String> {
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
        let tray = build("http://127.0.0.1:8766/".to_string()).unwrap();

        assert_eq!(
            tray.run().unwrap_err(),
            "tray UI is only available on Windows builds"
        );
    }
}
