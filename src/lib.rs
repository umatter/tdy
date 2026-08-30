//! tdy: pure SQL over messy files.
//!
//! Module map:
//! - [`spec`]     ParseSpec types — the contract everything shares
//! - [`sidecar`]  `<file>.tdy.toml` persistence + freshness
//! - [`sample`]   what sniffer and LLM get to see (rendered, decoded)
//! - [`sniff`]    tier-1 deterministic heuristics
//! - [`detect`]   log-line and fixed-width shape recognition for tier 1
//! - [`numfmt`]   which character is the decimal point (and proof, not guesswork)
//! - [`infer`]    tier-2 LLM inference with grammar-constrained decoding
//! - [`engine`]   ParseSpec + file -> Arrow RecordBatch
//! - [`provider`] DataFusion `messy()` UDTF, query running, output
//! - [`sqlscan`]  finding `messy('...')` in SQL without mistaking comments for code
//! - [`fileio`]   bounded reads, streaming hashes, atomic writes
//! - [`config`]   backend configuration resolution

pub mod config;
pub mod conform;
pub mod detect;
pub mod engine;
pub mod fileio;
pub mod infer;
pub mod numfmt;
pub mod provider;
pub mod sample;
pub mod sidecar;
pub mod sniff;
pub mod spec;
pub mod stream;
pub mod target;
pub mod sqlscan;
pub mod xlguard;
