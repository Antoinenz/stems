//! Vocal separation backends behind one small trait.
//!
//! Phase 1 ships the zero-latency DSP fallback only. ML backends (ONNX
//! in-process, sidecar, remote worker) implement the same trait with a
//! non-zero `latency_frames()` that the pipeline turns into a delay line.

#[cfg(feature = "onnx")]
pub mod align;
pub mod dsp;
#[cfg(feature = "onnx")]
pub mod mdx;
pub mod offline;
pub mod params;
pub mod remote;
#[cfg(feature = "onnx")]
pub mod stft;
pub mod stream;

pub use dsp::CentreCancel;
#[cfg(feature = "onnx")]
pub use mdx::{Device, MdxModel};
pub use offline::{CachedStemSeparator, OfflineSeparator, StemBuffer, StemStore};
pub use params::{MdxParams, PrimaryStem, known_model};
pub use stems_core::Separator;
pub use stream::{MdxSeparator, StreamOpts, WindowRunner};
