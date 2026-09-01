//! The console: one grammar for the plain REPL, the batch runner and the
//! workbench. See docs/design/2026-09-01-console-and-workbench.md.

pub mod parse;

pub use parse::{parse, Command, ParseError};
