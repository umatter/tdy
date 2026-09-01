//! The terminal UI for tdy, as a library so its logic can be tested without
//! a terminal.
//!
//! The rendering lives in `ui`; everything that *decides* something —
//! remedies, evidence, application state — lives beside it in plain modules
//! with plain tests. A TUI whose logic is only reachable through a screen is
//! a TUI nobody can check.

pub use tdy::target::TargetColumn;

pub mod app;
pub mod evidence;
pub mod remedy;
pub mod ui;
