//! The interactive terminal UI — hauksbee's default human-facing surface.
//!
//! `hauksbee run <board>` with no output-selecting flag, on a TTY, lands here.
//! The TUI is a **renderer over the existing structured-honest result**: it
//! calls the same bind / DRC / SI / lint paths the `--json`/text surfaces use
//! ([`app::build_state`]), so it can never disagree with the machine output.
//!
//! Module layout:
//! - [`state`]   — the pure, terminal-free model (findings, parts, nets, pane
//!                 focus, selection, verdict). Fully unit-testable with no PTY.
//! - [`cosim`]   — the background co-sim worker that streams incremental UART /
//!                 GPIO / progress updates to the UI over a channel.
//! - [`render`]  — ratatui drawing of the three panes + footer.
//! - [`app`]     — terminal lifecycle (alt-screen, raw mode, panic-safe restore)
//!                 and the event loop.

pub mod app;
pub mod cosim;
pub mod render;
pub mod state;

pub use app::{build_state, run};
pub use state::{AppState, Finding, Net, Pane, Part, PartStatus, Severity, Verdict};
