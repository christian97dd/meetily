//! Tells remote speakers apart. Every system-audio speech segment gets a voice embedding
//! (WeSpeaker CAM++, local ONNX) and is matched against the voices heard so far in the recording,
//! so "Others" becomes Speaker 1, 2, 3...
//!
//! Each VAD segment is treated as a single speaker: they are short speech bursts, and overlaps are rare
//! enough in calls that per-segment matching beats running a separate segmentation model.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use log::{info, warn};
use pyannote_rs::EmbeddingExtractor;
use tauri::{AppHandle, Manager, Runtime};

const MODEL_FILE: &str = "wespeaker_en_voxceleb_CAM++.onnx";
const MODEL_URL: &str =
    "https://github.com/thewh1teagle/pyannote-rs/releases/download/v0.1.0/wespeaker_en_voxceleb_CAM++.onnx";

/// Below ~1s of speech the embedding is too noisy to trust; such segments inherit the previous speaker.
const MIN_EMBEDDING_SAMPLES: usize = 16_000;
/// Only segments this long may register a new voice; shorter ones join the closest known voice.
/// Short bursts were splitting one person into several speakers.
const MIN_NEW_SPEAKER_SAMPLES: usize = 40_000;
/// Cosine similarity against a voice's running mean needed to reuse it. Call audio is compressed,
/// so the same person rarely scores as high as on clean recordings.
const SAME_SPEAKER_SIMILARITY: f32 = 0.4;
const MAX_SPEAKERS: usize = 10;

/// Known voices of one recording, each kept as the running mean of its (unit-length) embeddings.
struct SpeakerRegistry {
    centroids: Vec<(Vec<f32>, u32)>,
}

impl SpeakerRegistry {
    fn new() -> Self {
        Self { centroids: Vec::new() }
    }

    /// 1-based speaker id for `embedding`. `may_create` allows registering a new voice when none is similar.
    fn assign(&mut self, embedding: &[f32], may_create: bool) -> Option<usize> {
        let embedding = normalized(embedding)?;
        let best = self
            .centroids
            .iter()
            .enumerate()
            .map(|(index, (centroid, _))| (index, cosine(&embedding, centroid)))
            .max_by(|(_, a), (_, b)| a.total_cmp(b));

        let index = match best {
            Some((index, similarity)) if similarity >= SAME_SPEAKER_SIMILARITY => index,
            _ if may_create && self.centroids.len() < MAX_SPEAKERS => {
                self.centroids.push((embedding, 1));
                return Some(self.centroids.len());
            }
            // Not allowed to create (short segment or full): the closest voice is the best guess
            Some((index, _)) => return Some(index + 1),
            None => return None,
        };

        let (centroid, count) = &mut self.centroids[index];
        *count += 1;
        let weight = 1.0 / *count as f32;
        for (c, e) in centroid.iter_mut().zip(&embedding) {
            *c += (e - *c) * weight;
        }
        Some(index + 1)
    }
}

fn normalized(v: &[f32]) -> Option<Vec<f32>> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    (norm > f32::EPSILON).then(|| v.iter().map(|x| x / norm).collect())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    // `a` is unit length; centroids drift below it as they average
    if norm_b > f32::EPSILON { dot / norm_b } else { 0.0 }
}

static MODEL_PATH: OnceLock<PathBuf> = OnceLock::new();
static SESSION: Mutex<Option<RecordingDiarizer>> = Mutex::new(None);

struct RecordingDiarizer {
    extractor: EmbeddingExtractor,
    speakers: SpeakerRegistry,
    last_speaker: Option<usize>,
}

impl RecordingDiarizer {
    fn speaker_for(&mut self, samples: &[f32]) -> Option<usize> {
        if samples.len() < MIN_EMBEDDING_SAMPLES {
            return self.last_speaker;
        }

        let pcm: Vec<i16> = samples.iter().map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).collect();
        let embedding: Vec<f32> = match self.extractor.compute(&pcm) {
            Ok(embedding) => embedding.collect(),
            Err(e) => {
                warn!("Diarization: failed to compute voice embedding: {}", e);
                return self.last_speaker;
            }
        };

        let speaker = self.speakers.assign(&embedding, samples.len() >= MIN_NEW_SPEAKER_SAMPLES);
        if speaker.is_some() {
            self.last_speaker = speaker;
        }
        speaker
    }
}

/// Sets where the model lives and downloads it in the background if missing.
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    let Ok(app_data_dir) = app.path().app_data_dir() else {
        warn!("Diarization: app data dir unavailable, speakers won't be separated");
        return;
    };
    let path = app_data_dir.join("models").join("diarization").join(MODEL_FILE);
    let _ = MODEL_PATH.set(path.clone());

    if !path.exists() {
        tauri::async_runtime::spawn(async move {
            match download_model(&path).await {
                Ok(()) => info!("Diarization: model downloaded to {}", path.display()),
                Err(e) => warn!("Diarization: model download failed, speakers won't be separated: {}", e),
            }
        });
    }
}

async fn download_model(path: &PathBuf) -> Result<(), String> {
    let dir = path.parent().ok_or("invalid model path")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let response = reqwest::get(MODEL_URL).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;

    // Write aside and rename so an interrupted download never leaves a truncated model behind
    let partial = path.with_extension("onnx.part");
    std::fs::write(&partial, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&partial, path).map_err(|e| e.to_string())
}

/// Starts with no known voices. Without the model, remote speech stays as plain "system".
pub fn start_session() {
    let diarizer = MODEL_PATH
        .get()
        .filter(|path| path.exists())
        .and_then(|path| match EmbeddingExtractor::new(path) {
            Ok(extractor) => Some(RecordingDiarizer {
                extractor,
                speakers: SpeakerRegistry::new(),
                last_speaker: None,
            }),
            Err(e) => {
                warn!("Diarization: failed to load model: {}", e);
                None
            }
        });

    info!("Diarization: {}", if diarizer.is_some() { "enabled for this recording" } else { "unavailable" });
    if let Ok(mut session) = SESSION.lock() {
        *session = diarizer;
    }
}

pub fn end_session() {
    if let Ok(mut session) = SESSION.lock() {
        *session = None;
    }
}

/// Speaker key for a system-audio segment: "system-N", or "system" when voices can't be told apart.
pub fn system_speaker_key(samples: &[f32]) -> String {
    let speaker = SESSION.lock().ok().and_then(|mut session| session.as_mut()?.speaker_for(samples));
    match speaker {
        Some(id) => format!("system-{}", id),
        None => "system".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(seed: f32, noise: f32) -> Vec<f32> {
        (0..8).map(|i| ((i as f32 + seed) * 1.7).sin() + noise * ((i as f32 * 3.1).cos())).collect()
    }

    #[test]
    fn the_same_voice_with_noise_keeps_one_id() {
        let mut registry = SpeakerRegistry::new();
        assert_eq!(registry.assign(&voice(0.0, 0.0), true), Some(1));
        assert_eq!(registry.assign(&voice(0.0, 0.3), true), Some(1));
        assert_eq!(registry.assign(&voice(0.0, -0.3), true), Some(1));
    }

    #[test]
    fn a_different_voice_gets_a_new_id_when_allowed() {
        let mut registry = SpeakerRegistry::new();
        assert_eq!(registry.assign(&voice(0.0, 0.0), true), Some(1));
        assert_eq!(registry.assign(&[1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0], true), Some(2));
    }

    #[test]
    fn short_segments_join_the_closest_voice_instead_of_creating_one() {
        let mut registry = SpeakerRegistry::new();
        registry.assign(&voice(0.0, 0.0), true);
        assert_eq!(registry.assign(&[1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0], false), Some(1));
        assert_eq!(registry.centroids.len(), 1);
    }

    #[test]
    fn nothing_is_assigned_before_any_voice_is_known_unless_creation_is_allowed() {
        let mut registry = SpeakerRegistry::new();
        assert_eq!(registry.assign(&voice(0.0, 0.0), false), None);
        assert_eq!(registry.assign(&[0.0; 8], true), None);
    }

    #[test]
    fn stops_creating_voices_at_the_limit() {
        let mut registry = SpeakerRegistry::new();
        for i in 0..MAX_SPEAKERS {
            let mut one_hot = vec![0.0; MAX_SPEAKERS + 1];
            one_hot[i] = 1.0;
            assert_eq!(registry.assign(&one_hot, true), Some(i + 1));
        }
        let mut extra = vec![0.0; MAX_SPEAKERS + 1];
        extra[MAX_SPEAKERS] = 1.0;
        assert!(registry.assign(&extra, true).unwrap() <= MAX_SPEAKERS);
        assert_eq!(registry.centroids.len(), MAX_SPEAKERS);
    }
}
