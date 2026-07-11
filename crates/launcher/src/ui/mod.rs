//! `LauncherUi` port (ADR 0001 D2): `main.rs`/`runner.rs` only ever talk to
//! this trait, so the concrete UI toolkit can be replaced later without
//! touching runner, config, or MCP-hosting code.

pub mod tray;

pub trait LauncherUi {
    /// Runs the UI's event loop on the calling thread until the user closes
    /// it. Returns once the launcher should shut down.
    fn run(&self) -> Result<(), String>;
}
