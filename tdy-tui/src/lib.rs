//! The terminal UI for tdy, as a library so its logic can be tested without
//! a terminal.
//!
//! The rendering lives in `wb_ui`; everything that *decides* something —
//! remedies, evidence, the frame's own state (`workbench`) — lives beside it
//! in plain modules with plain tests. A TUI whose logic is only reachable
//! through a screen is a TUI nobody can check.

pub use tdy::target::TargetColumn;
pub use tdy::evidence;

pub mod browser;
pub mod mark;
pub mod remedy;
pub mod wb_ui;
pub mod workbench;
