//! Reading FLAC instrumentals: lossless and about half the size of WAV,
//! which matters when they come down from a rented GPU.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use stems_core::{CHANNELS, SAMPLE_RATE};
use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Decode a stereo 44.1 kHz FLAC to interleaved f32. Other layouts are an
/// error: instrumentals must line up sample for sample with the mix.
pub fn read_pcm(path: &Path) -> Result<Vec<f32>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| format!("probe {}", path.display()))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio in {}", path.display()))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("{}: no codec parameters", path.display()))?
        .clone();
    let rate = params.sample_rate.unwrap_or(0);
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(0);
    if rate != SAMPLE_RATE || channels != CHANNELS as usize {
        bail!(
            "{}: expected {CHANNELS}ch {SAMPLE_RATE} Hz, got {channels}ch {rate} Hz",
            path.display()
        );
    }
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("create FLAC decoder")?;
    let mut pcm = Vec::new();
    let mut chunk: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = decoder
            .decode(&packet)
            .with_context(|| format!("decode {}", path.display()))?;
        chunk.resize(buf.samples_interleaved(), f32::MID);
        buf.copy_to_slice_interleaved(&mut chunk);
        pcm.extend_from_slice(&chunk);
    }
    Ok(pcm)
}
