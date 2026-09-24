//! Tells remote speakers apart. Every system-audio speech segment gets a voice embedding
//! (WeSpeaker CAM++, local ONNX) and is matched against the voices heard so far in the recording,
//! so "Others" becomes Speaker 1, 2, 3...
//!
//! Each VAD segment is treated as a single speaker: they are short speech bursts, and overlaps are rare
//! enough in calls that per-segment matching beats running a separate segmentation model.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use log::{info, warn};
use pyannote_rs::{EmbeddingExtractor, EmbeddingManager};
use tauri::{AppHandle, Manager, Runtime};

const MODEL_FILE: &str = "wespeaker_en_voxceleb_CAM++.onnx";
const MODEL_URL: &str =
    "https://github.com/thewh1teagle/pyannote-rs/releases/download/v0.1.0/wespeaker_en_voxceleb_CAM++.onnx";

/// Below ~1s of speech the embedding is too noisy to trust; such segments inherit the previous speaker.
const MIN_EMBEDDING_SAMPLES: usize = 16_000;
/// Cosine similarity needed to reuse a known voice instead of registering a new one.
const SAME_SPEAKER_SIMILARITY: f32 = 0.5;
const MAX_SPEAKERS: usize = 10;

static MODEL_PATH: OnceLock<PathBuf> = OnceLock::new();
static SESSION: Mutex<Option<RecordingDiarizer>> = Mutex::new(None);

struct RecordingDiarizer {
    extractor: EmbeddingExtractor,
    speakers: EmbeddingManager,
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

        // search_speaker gives up once MAX_SPEAKERS voices exist; then settle for the closest one
        let speaker = self
            .speakers
            .search_speaker(embedding.clone(), SAME_SPEAKER_SIMILARITY)
            .or_else(|| self.speakers.get_best_speaker_match(embedding).ok());
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
                speakers: EmbeddingManager::new(MAX_SPEAKERS),
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
