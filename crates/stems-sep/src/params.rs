//! MDX-Net model geometry and the registry of known UVR models. No ONNX
//! dependency, so remote clients can use it too.

use serde::{Deserialize, Serialize};

/// Which stem the model's output is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryStem {
    Instrumental,
    Vocals,
}

/// STFT / tensor geometry of an MDX-Net model (from UVR's model registry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MdxParams {
    pub n_fft: usize,
    pub hop: usize,
    pub dim_f: usize,
    pub dim_t: usize,
    pub primary_stem: PrimaryStem,
    pub compensate: f32,
}

impl MdxParams {
    /// Samples per model window.
    pub fn chunk_size(&self) -> usize {
        self.hop * (self.dim_t - 1)
    }

    /// Samples trimmed at each end of a window (edge artefacts).
    pub fn trim(&self) -> usize {
        self.n_fft / 2
    }

    /// Valid output samples produced per window.
    pub fn gen_size(&self) -> usize {
        self.chunk_size() - 2 * self.trim()
    }
}

/// Known models: download source and parameters (from UVR's registry).
pub fn known_model(name: &str) -> Option<(&'static str, MdxParams)> {
    let base = "https://github.com/TRvlvr/model_repo/releases/download/all_public_uvr_models";
    match name {
        "UVR-MDX-NET-Inst_HQ_3" => Some((
            base,
            MdxParams {
                n_fft: 6144,
                hop: 1024,
                dim_f: 3072,
                dim_t: 256,
                primary_stem: PrimaryStem::Instrumental,
                compensate: 1.022,
            },
        )),
        "Kim_Vocal_2" => Some((
            base,
            MdxParams {
                n_fft: 7680,
                hop: 1024,
                dim_f: 3072,
                dim_t: 256,
                primary_stem: PrimaryStem::Vocals,
                compensate: 1.009,
            },
        )),
        _ => None,
    }
}
