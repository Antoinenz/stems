//! librespot "mixer" that only *stores* the Connect volume. Audio is left
//! at unity so the separator sees the original signal; the actual volume is
//! applied by the stems sink via `SourceEvent::Volume`.

use std::sync::atomic::{AtomicU16, Ordering};

use librespot_playback::mixer::{
    Mixer, MixerConfig, NoOpVolume as LibrespotNoOpVolume, VolumeGetter,
};

pub struct VolumeStore {
    volume: AtomicU16,
}

impl VolumeStore {
    pub fn new(percent: u16) -> Self {
        let v = (percent.min(100) as u32 * u16::MAX as u32 / 100) as u16;
        Self {
            volume: AtomicU16::new(v),
        }
    }
}

impl Mixer for VolumeStore {
    fn open(_config: MixerConfig) -> Result<Self, librespot_core::Error> {
        Ok(Self::new(70))
    }

    fn volume(&self) -> u16 {
        self.volume.load(Ordering::Relaxed)
    }

    fn set_volume(&self, volume: u16) {
        self.volume.store(volume, Ordering::Relaxed);
    }
}

pub struct NoOpVolume;

impl NoOpVolume {
    pub fn boxed() -> Box<dyn VolumeGetter + Send> {
        Box::new(LibrespotNoOpVolume)
    }
}
