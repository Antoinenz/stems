//! Audio plumbing: a lock-free ring, a mixer, and output sinks.
//!
//! Everything here operates on f32 interleaved stereo at
//! [`stems_core::SAMPLE_RATE`]. Nothing on the sink side allocates or locks
//! once the stream is running.

pub mod mixer;
pub mod processor;
pub mod resample;
pub mod ring;
pub mod sink;

pub use mixer::Mixer;
pub use processor::{Processor, ProcessorControl};
pub use resample::StreamResampler;
pub use ring::{AudioRing, Consumer, FlushSignal, Producer};
pub use sink::{SinkKind, cpal::CpalSink, null::NullSink, wav::WavSink};
