//! Writes the microphone and system audio as two separate 16kHz mono WAV files next to the mixed
//! recording, so post-meeting re-transcription can still tell who said what. 16kHz is what the
//! ASR engines consume, and keeps each track around 115 MB per hour.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use hound::{SampleFormat, WavSpec, WavWriter};
use log::{info, warn};

use super::recording_state::DeviceType;

pub const MIC_TRACK_FILE: &str = "mic.wav";
pub const SYSTEM_TRACK_FILE: &str = "system.wav";
const TRACK_SAMPLE_RATE: u32 = 16_000;

static TRACKS: Mutex<Option<TrackWriters>> = Mutex::new(None);
/// Input windows received since the tracks were opened; the recording watchdog uses it to notice
/// audio that stopped arriving.
static WINDOWS_WRITTEN: AtomicU64 = AtomicU64::new(0);

struct TrackWriters {
    mic: WavWriter<BufWriter<File>>,
    system: WavWriter<BufWriter<File>>,
    input_sample_rate: u32,
}

fn create_writer(path: &Path) -> hound::Result<WavWriter<BufWriter<File>>> {
    WavWriter::create(
        path,
        WavSpec { channels: 1, sample_rate: TRACK_SAMPLE_RATE, bits_per_sample: 16, sample_format: SampleFormat::Int },
    )
}

/// Averages each group of `ratio` samples: a cheap low-pass plus decimation, enough for speech.
fn downsample(samples: &[f32], ratio: usize) -> impl Iterator<Item = f32> + '_ {
    samples.chunks(ratio).map(|group| group.iter().sum::<f32>() / group.len() as f32)
}

fn to_pcm(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Opens both track files in the meeting folder. Only integer multiples of 16kHz are supported
/// (the pipeline runs at 48kHz); anything else skips track recording.
pub fn start(meeting_folder: &Path, input_sample_rate: u32) {
    if input_sample_rate % TRACK_SAMPLE_RATE != 0 {
        warn!("Track recorder: unsupported input rate {}Hz, separate tracks disabled", input_sample_rate);
        return;
    }
    let writers = create_writer(&meeting_folder.join(MIC_TRACK_FILE))
        .and_then(|mic| Ok((mic, create_writer(&meeting_folder.join(SYSTEM_TRACK_FILE))?)));
    match writers {
        Ok((mic, system)) => {
            if let Ok(mut tracks) = TRACKS.lock() {
                *tracks = Some(TrackWriters { mic, system, input_sample_rate });
            }
            WINDOWS_WRITTEN.store(0, Ordering::Release);
            info!("Track recorder: writing separate mic/system tracks to {}", meeting_folder.display());
        }
        Err(e) => warn!("Track recorder: failed to create track files: {}", e),
    }
}

/// Appends one window of a source. Both sources receive equal-length windows, so the tracks stay aligned.
pub fn write(device_type: &DeviceType, samples: &[f32]) {
    let Ok(mut tracks) = TRACKS.lock() else { return };
    let Some(tracks) = tracks.as_mut() else { return };
    WINDOWS_WRITTEN.fetch_add(1, Ordering::AcqRel);
    let ratio = (tracks.input_sample_rate / TRACK_SAMPLE_RATE) as usize;
    let writer = match device_type {
        DeviceType::Microphone => &mut tracks.mic,
        DeviceType::System => &mut tracks.system,
    };
    for sample in downsample(samples, ratio) {
        if let Err(e) = writer.write_sample(to_pcm(sample)) {
            warn!("Track recorder: write failed, stopping {:?} track: {}", device_type, e);
            break;
        }
    }
}

/// Closes both files (writing their WAV headers). Safe to call when no tracks are open.
pub fn finish() {
    let Some(tracks) = TRACKS.lock().ok().and_then(|mut tracks| tracks.take()) else { return };
    for (name, writer) in [("mic", tracks.mic), ("system", tracks.system)] {
        if let Err(e) = writer.finalize() {
            warn!("Track recorder: failed to finalize {} track: {}", name, e);
        }
    }
    info!("Track recorder: tracks finalized");
}

/// How many audio windows have reached the tracks so far, or None when no tracks are being written
/// (audio saving off), in which case progress can't be observed.
pub fn progress() -> Option<u64> {
    let open = TRACKS.lock().ok().is_some_and(|tracks| tracks.is_some());
    open.then(|| WINDOWS_WRITTEN.load(Ordering::Acquire))
}

/// Paths of both tracks when a meeting has them.
pub fn existing_tracks(meeting_folder: &Path) -> Option<(PathBuf, PathBuf)> {
    let mic = meeting_folder.join(MIC_TRACK_FILE);
    let system = meeting_folder.join(SYSTEM_TRACK_FILE);
    (mic.exists() && system.exists()).then_some((mic, system))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsampling_averages_each_group() {
        let out: Vec<f32> = downsample(&[0.0, 0.3, 0.6, 1.0, 1.0, 1.0, 0.5], 3).collect();
        assert_eq!(out.len(), 3);
        assert!((out[0] - 0.3).abs() < 1e-6);
        assert!((out[1] - 1.0).abs() < 1e-6);
        assert!((out[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn pcm_conversion_clamps() {
        assert_eq!(to_pcm(2.0), i16::MAX);
        assert_eq!(to_pcm(-2.0), -i16::MAX);
        assert_eq!(to_pcm(0.0), 0);
    }

    #[test]
    fn writes_aligned_tracks_that_can_be_read_back() {
        let dir = tempfile::tempdir().unwrap();
        start(dir.path(), 48_000);
        write(&DeviceType::Microphone, &vec![0.5; 4_800]);
        write(&DeviceType::System, &vec![-0.5; 4_800]);
        finish();

        let (mic, system) = existing_tracks(dir.path()).expect("both tracks exist");
        for (path, expected) in [(mic, 0.5f32), (system, -0.5f32)] {
            let mut reader = hound::WavReader::open(path).unwrap();
            assert_eq!(reader.spec().sample_rate, 16_000);
            let samples: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
            assert_eq!(samples.len(), 1_600);
            assert!((samples[0] as f32 / i16::MAX as f32 - expected).abs() < 1e-3);
        }
    }
}
