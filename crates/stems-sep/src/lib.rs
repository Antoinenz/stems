//! Vocal separation backends behind one small trait.
//!
//! Phase 1 ships the zero-latency DSP fallback only. ML backends (ONNX
//! in-process, sidecar, remote worker) implement the same trait with a
//! non-zero `latency_frames()` that the pipeline turns into a delay line.

pub mod dsp;

pub use dsp::CentreCancel;
pub use stems_core::Separator;
