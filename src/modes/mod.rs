pub mod countdown;
pub mod install;
pub mod popup;
pub mod rollback;
// Diagnostic-only mode reachable via `--sandbox --try <preset>`.
// Kept for future debug sessions ; final decision (strip vs
// cfg-gate vs Cargo feature) deferred to the pre-v1 cleanup pass
// alongside the --debug default flip.
pub mod sandbox;
pub mod service;
pub mod uninstall;
pub mod watchdog;

#[cfg(windows)]
pub mod config_first_run;

// Multi-tab passcode-gated config UI. The whole module compiles on
// Linux so the pure `config::lockout` state-machine tests exercise on
// every `cargo test` ; the eframe / rfd / IPC bits are gated inside
// under `#[cfg(windows)]`.
pub mod config;
