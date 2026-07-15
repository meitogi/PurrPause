// Module index for the passcode-gated multi-tab config UI.
//
// The `lockout` submodule is pure state-machine logic (no egui, no
// I/O) and compiles on every platform so its tests run under Linux
// `cargo test`. The other submodules (eframe App, egui tab bodies,
// rfd file picker, IPC client) are Windows-only.

pub mod lockout;

#[cfg(windows)]
pub(crate) mod decoration;
#[cfg(windows)]
mod passcode;
#[cfg(windows)]
mod tabs;
#[cfg(windows)]
mod app;

#[cfg(windows)]
pub use app::{run, Action};

#[cfg(not(windows))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("config UI is Windows-only")
}
