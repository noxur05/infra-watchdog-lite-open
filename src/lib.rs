//! Library entry point. Exposes pure/utility modules so integration tests
//! under `tests/` can exercise them without pulling the whole binary.
//!
//! The bot itself still runs from `main.rs` — this file just makes the
//! parsing/formatting helpers reachable as `use vpn_watchdog::*`.

pub mod utils;
pub mod time_utils;
