//! Word timing results, shared by the local aligner and the worker
//! protocol (which must build without ONNX on the client side).

use serde::{Deserialize, Serialize};

/// A word's span inside the audio handed to an aligner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordSpan {
    pub start_ms: u32,
    pub end_ms: u32,
    /// Mean log-probability of the word's letters along the path.
    pub score: f32,
}
