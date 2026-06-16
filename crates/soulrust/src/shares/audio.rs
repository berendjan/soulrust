//! Audio-metadata extraction for shared files, producing the `(type, value)`
//! attribute pairs Soulseek carries in browse and search file entries.
//!
//! The wire mapping ([`AudioMeta::to_attributes`]) is pure and fully tested; the
//! extraction ([`read`]) wraps `lofty` and is best-effort — a non-audio or
//! unreadable file is still shared, just without attributes.

use std::path::Path;

/// Soulseek file-attribute type codes (Nicotine+ `FileAttribute` /
/// Soulseek.NET `FileAttributeType`). Sample size / bit depth is code 5.
mod attr {
    pub const BITRATE: u32 = 0;
    pub const DURATION: u32 = 1;
    pub const SAMPLE_RATE: u32 = 4;
    pub const BIT_DEPTH: u32 = 5;
}

/// The audio properties we advertise on the wire, all optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioMeta {
    pub duration_secs: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub bit_depth: Option<u32>,
}

impl AudioMeta {
    /// The Soulseek attribute pairs for these properties, following Nicotine+'s
    /// convention: lossless files (those reporting a bit depth) advertise
    /// duration + sample rate + bit depth; lossy files advertise bitrate +
    /// duration. Pairs are ordered by type code for determinism, and any
    /// property we don't have is simply omitted — an [`AudioMeta`] with nothing
    /// usable yields an empty list (i.e. "no attributes").
    pub fn to_attributes(&self) -> Vec<(u32, u32)> {
        let mut attrs = Vec::new();
        if self.bit_depth.is_some() {
            // Lossless (FLAC/WAV/ALAC/…): duration, sample rate, bit depth.
            if let Some(d) = self.duration_secs {
                attrs.push((attr::DURATION, d));
            }
            if let Some(sr) = self.sample_rate_hz {
                attrs.push((attr::SAMPLE_RATE, sr));
            }
            if let Some(bd) = self.bit_depth {
                attrs.push((attr::BIT_DEPTH, bd));
            }
        } else {
            // Lossy / unknown (MP3/AAC/OGG/…): bitrate, duration.
            if let Some(br) = self.bitrate_kbps {
                attrs.push((attr::BITRATE, br));
            }
            if let Some(d) = self.duration_secs {
                attrs.push((attr::DURATION, d));
            }
        }
        attrs
    }
}

/// Read audio properties from `path`, or `None` if it is not a readable audio
/// file (non-audio, unsupported format, or I/O error). All failures are
/// non-fatal: the file is still shared, just without attributes.
pub fn read(path: &Path) -> Option<AudioMeta> {
    use lofty::file::AudioFile;
    let tagged = lofty::read_from_path(path).ok()?;
    let props = tagged.properties();
    let duration = props.duration();
    Some(AudioMeta {
        // A zero duration means lofty couldn't determine it — treat as absent.
        duration_secs: (!duration.is_zero()).then_some(duration.as_secs() as u32),
        bitrate_kbps: props.audio_bitrate().or_else(|| props.overall_bitrate()),
        sample_rate_hz: props.sample_rate(),
        bit_depth: props.bit_depth().map(u32::from),
    })
}

/// The Soulseek attribute pairs for a file on disk — empty if it is not a
/// readable audio file.
pub fn attributes_of(path: &Path) -> Vec<(u32, u32)> {
    read(path).map(|m| m.to_attributes()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn lossy_advertises_bitrate_and_duration() {
        let meta = AudioMeta {
            duration_secs: Some(213),
            bitrate_kbps: Some(320),
            sample_rate_hz: Some(44_100),
            bit_depth: None, // lossy -> no bit depth
        };
        // Bitrate (0) then duration (1); sample rate is not advertised for lossy.
        assert_eq!(meta.to_attributes(), vec![(0, 320), (1, 213)]);
    }

    #[test]
    fn lossless_advertises_duration_samplerate_bitdepth() {
        let meta = AudioMeta {
            duration_secs: Some(180),
            bitrate_kbps: Some(900), // present but not advertised for lossless
            sample_rate_hz: Some(44_100),
            bit_depth: Some(16),
        };
        assert_eq!(meta.to_attributes(), vec![(1, 180), (4, 44_100), (5, 16)]);
    }

    #[test]
    fn missing_properties_are_omitted() {
        // Lossy with only a bitrate.
        let only_bitrate = AudioMeta { bitrate_kbps: Some(128), ..Default::default() };
        assert_eq!(only_bitrate.to_attributes(), vec![(0, 128)]);

        // Lossless with no duration: sample rate + bit depth only.
        let no_duration =
            AudioMeta { sample_rate_hz: Some(48_000), bit_depth: Some(24), ..Default::default() };
        assert_eq!(no_duration.to_attributes(), vec![(4, 48_000), (5, 24)]);

        // Nothing usable -> no attributes.
        assert!(AudioMeta::default().to_attributes().is_empty());
    }

    fn temp_path(tag: &str, ext: &str) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("soulrust-audio-{}-{n}-{tag}.{ext}", std::process::id()))
    }

    /// A minimal 16-bit PCM WAV: `secs` seconds of silence at `sample_rate` Hz,
    /// mono — enough for lofty to report sample rate, bit depth, and duration.
    fn write_wav(path: &Path, sample_rate: u32, secs: u32) {
        let bits = 16u16;
        let channels = 1u16;
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits) / 8;
        let block_align = channels * bits / 8;
        let data_len = byte_rate * secs;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(wav.len() + data_len as usize, 0); // silent samples
        std::fs::write(path, wav).unwrap();
    }

    #[test]
    fn reads_real_wav_properties_end_to_end() {
        let path = temp_path("pcm", "wav");
        write_wav(&path, 8_000, 1); // 1 second, 8 kHz, 16-bit
        let meta = read(&path).expect("a valid WAV should parse");
        assert_eq!(meta.sample_rate_hz, Some(8_000));
        assert_eq!(meta.bit_depth, Some(16));
        assert_eq!(meta.duration_secs, Some(1));
        // Lossless mapping: duration, sample rate, bit depth.
        assert_eq!(meta.to_attributes(), vec![(1, 1), (4, 8_000), (5, 16)]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn non_audio_file_yields_no_attributes() {
        let path = temp_path("notes", "txt");
        std::fs::write(&path, b"this is not audio").unwrap();
        assert!(read(&path).is_none(), "a text file is not audio");
        assert!(attributes_of(&path).is_empty());
        let _ = std::fs::remove_file(&path);

        // A path that doesn't exist is also just "no attributes", not a panic.
        assert!(attributes_of(Path::new("/no/such/file.mp3")).is_empty());
    }
}
