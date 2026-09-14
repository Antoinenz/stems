//! Enrichment: turn a [`TrackRef`] into lyrics (and later: video, stems).
//!
//! Phase 1 provider is [LRCLIB](https://lrclib.net) — free, keyless,
//! line-synced LRC for a large catalogue.

pub mod lrc;
pub mod lrclib;

pub use lrc::parse_lrc;
pub use lrclib::LrcLib;
