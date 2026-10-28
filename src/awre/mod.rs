//! AWRE — Automatic Wireless Recognition Engine.
//!
//! Given a batch of demodulated message bit vectors, infers the protocol
//! structure: preamble/sync alignment, then per-field engines (length,
//! sequence number, address, checksum) producing scored `CommonRange`s,
//! orchestrated by `format_finder`.

pub mod common_range;
pub mod histogram;
pub mod preprocessor;
pub mod util;

pub use common_range::{CommonRange, RangeType};
