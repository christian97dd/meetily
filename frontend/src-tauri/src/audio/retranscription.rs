// Retranscription module - allows re-processing stored audio with different settings

use crate::audio::decoder::decode_audio_file;
use crate::audio::vad::get_speech_chunks_with_progress;
use super::common::{create_transcript_segments, split_segment_at_silence, write_transcripts_json};
use super::constants::AUDIO_EXTENSIONS;
use crate::config::{DEFAULT_WHISPER_MODEL, DEFAULT_PARAKEET_MODEL};
use crate::database::repositories::meeting::MeetingsRepository;
use crate::parakeet_engine::ParakeetEngine;
use crate::state::AppState;
use crate::whisper_engine::WhisperEngine;
use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// Global flag to track if retranscription is in progress
static RETRANSCRIPTION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Global flag to signal cancellation
static RETRANSCRIPTION_CANCELLED: AtomicBool = AtomicBool::new(false);

/// RAII guard for RETRANSCRIPTION_IN_PROGRESS flag
/// Ensures flag is cleared even if retranscription panics or returns early
struct RetranscriptionGuard;

impl RetranscriptionGuard {
    /// Create guard and set flag atomically
    fn acquire() -> Result<Self, String> {
        if RETRANSCRIPTION_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("Retranscription already in progress".to_string());
        }
        Ok(RetranscriptionGuard)
    }
}

impl Drop for RetranscriptionGuard {
    fn drop(&mut self) {
        RETRANSCRIPTION_IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

/// VAD redemption time in milliseconds - bridges natural pauses in speech
/// Batch processing needs longer redemption (2000ms) than the live pipeline
/// (500ms) because the entire file is processed at once by VAD with no
/// latency requirement, and short redemption fragments speech at every
/// natural sentence/topic pause (500ms-2s)
const VAD_REDEMPTION_TIME_MS: u32 = 2000;

/// Progress update emitted during retranscription
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionProgress {
    pub meeting_id: String,
    pub stage: String, // "decoding", "transcribing", "saving"
    pub progress_percentage: u32,
    pub message: String,
}

/// Result of retranscription
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionResult {
    pub meeting_id: String,
    pub segments_count: usize,
    pub duration_seconds: f64,
    pub language: Option<String>,
}

/// Error during retranscription
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionError {
    pub meeting_id: String,
    pub error: String,
}

/// Asks a running re-transcription to stop and waits for it to let go of the engine, so a recording
/// that's starting doesn't share the Whisper engine with it. Returns false if it didn't stop in time.
pub async fn cancel_and_wait(timeout: std::time::Duration) -> bool {
    if !is_retranscription_in_progress() {
        return true;
    }
    info!("Cancelling the running re-transcription: a recording is starting");
    cancel_retranscription();
    let deadline = tokio::time::Instant::now() + timeout;
    while is_retranscription_in_progress() {
        if tokio::time::Instant::now() >= deadline {
            warn!("Re-transcription did not stop within {:?}", timeout);
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    true
}

/// Check if retranscription is currently in progress
pub fn is_retranscription_in_progress() -> bool {
    RETRANSCRIPTION_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Cancel ongoing retranscription
pub fn cancel_retranscription() {
    RETRANSCRIPTION_CANCELLED.store(true, Ordering::SeqCst);
}

/// Start retranscription of a meeting's audio
pub async fn start_retranscription<R: Runtime>(
    app: AppHandle<R>,
    meeting_id: String,
    meeting_folder_path: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<RetranscriptionResult> {
    // Loading a batch model would swap it under the live recording's engine
    if super::recording_commands::is_recording().await {
        return Err(anyhow!("Can't re-transcribe while recording"));
    }

    // Acquire guard - ensures flag is cleared even on panic/early return
    let _guard = RetranscriptionGuard::acquire().map_err(|e| anyhow!(e))?;

    // Reset cancellation flag
    RETRANSCRIPTION_CANCELLED.store(false, Ordering::SeqCst);

    let use_parakeet = provider.as_deref() == Some("parakeet");
    let result = run_retranscription(app.clone(), meeting_id.clone(), meeting_folder_path, language, model, provider).await;

    // Unload the engine after the batch job (success, failure, or cancellation)
    super::common::unload_engine_after_batch(use_parakeet).await;

    // Guard will automatically clear flag on drop
    // No need for manual: RETRANSCRIPTION_IN_PROGRESS.store(false, Ordering::SeqCst);

    match &result {
        Ok(res) => {
            let _ = app.emit(
                "retranscription-complete",
                serde_json::json!({
                    "meeting_id": res.meeting_id,
                    "segments_count": res.segments_count,
                    "duration_seconds": res.duration_seconds,
                    "language": res.language
                }),
            );
        }
        Err(e) => {
            let _ = app.emit(
                "retranscription-error",
                RetranscriptionError {
                    meeting_id: meeting_id.clone(),
                    error: e.to_string(),
                },
            );
        }
    }

    result
}

/// Find audio file in meeting folder
/// Tries common names first, then scans for any file with an audio extension
fn find_audio_file(folder: &Path) -> Result<PathBuf> {
    let candidates = [
        "audio.mp4", "audio.m4a", "audio.wav", "audio.mp3",
        "audio.flac", "audio.ogg", "recording.mp4",
        "audio.mkv", "audio.webm", "audio.wma",
    ];

    for name in candidates {
        let path = folder.join(name);
        if path.exists() {
            return Ok(path);
        }
    }

    // Fallback: scan folder for any file with an audio extension
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                let ext = ext.to_string_lossy().to_lowercase();
                if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                    return Ok(path);
                }
            }
        }
    }

    Err(anyhow!("No audio file found in: {}", folder.display()))
}

/// Which recording a batch of audio comes from, deciding the speaker of its segments.
#[derive(Debug, Clone, Copy, PartialEq)]
enum AudioSource {
    /// Pre-split meetings and imports: one mixed track, speakers unknown
    Mixed,
    Mic,
    System,
}

/// Slice of the overall 0-100 progress bar a step reports into.
#[derive(Debug, Clone, Copy)]
struct ProgressSpan {
    start: u32,
    len: u32,
}

impl ProgressSpan {
    fn at(&self, local_percent: f32) -> u32 {
        self.start + (local_percent.clamp(0.0, 100.0) / 100.0 * self.len as f32) as u32
    }
}

enum BatchEngine {
    Whisper(Arc<WhisperEngine>),
    Parakeet(Arc<ParakeetEngine>),
}

/// (text, start_ms, end_ms, speaker)
type SpeakerTranscript = (String, f64, f64, Option<String>);

fn check_cancelled() -> Result<()> {
    if RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst) {
        return Err(anyhow!("Retranscription cancelled"));
    }
    Ok(())
}

/// Decodes one audio file, splits it into speech segments and transcribes them.
/// `label` names each segment's speaker from its samples. Returns transcripts and the audio duration.
async fn transcribe_source<R: Runtime>(
    app: &AppHandle<R>,
    meeting_id: &str,
    audio_path: &Path,
    source: AudioSource,
    engine: &BatchEngine,
    language: &Option<String>,
    span: ProgressSpan,
    mut label: impl FnMut(&[f32]) -> Option<String>,
) -> Result<(Vec<SpeakerTranscript>, f64)> {
    let what = match source {
        AudioSource::Mixed => "audio",
        AudioSource::Mic => "microphone track",
        AudioSource::System => "system audio track",
    };
    emit_progress(app, meeting_id, "decoding", span.at(0.0), &format!("Decoding {}...", what));
    check_cancelled()?;

    // Decode and resample (CPU-intensive, run in blocking task)
    let path_for_decode = audio_path.to_path_buf();
    let decoded = tokio::task::spawn_blocking(move || decode_audio_file(&path_for_decode))
        .await
        .map_err(|e| anyhow!("Decode task panicked: {}", e))??;
    let duration_seconds = decoded.duration_seconds;
    info!(
        "Decoded {}: {:.2}s, {}Hz, {} channels",
        audio_path.display(), duration_seconds, decoded.sample_rate, decoded.channels
    );
    let audio_samples = tokio::task::spawn_blocking(move || decoded.to_whisper_format())
        .await
        .map_err(|e| anyhow!("Resample task panicked: {}", e))?;
    check_cancelled()?;

    // Use VAD to find natural speech boundaries (same approach as live transcription).
    // Run in a blocking task: for large files (35+ minutes) VAD can take several minutes.
    let app_for_vad = app.clone();
    let meeting_id_for_vad = meeting_id.to_string();
    let speech_segments = tokio::task::spawn_blocking(move || {
        get_speech_chunks_with_progress(&audio_samples, VAD_REDEMPTION_TIME_MS, |vad_progress, segments_found| {
            emit_progress(
                &app_for_vad,
                &meeting_id_for_vad,
                "vad",
                span.at(5.0 + vad_progress as f32 * 0.1),
                &format!("Detecting speech segments... {}% ({} found)", vad_progress, segments_found),
            );
            // Return false to cancel if cancellation requested
            !RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst)
        })
    })
    .await
    .map_err(|e| anyhow!("VAD task panicked: {}", e))?
    .map_err(|e| anyhow!("VAD processing failed: {}", e))?;
    info!(
        "VAD detected {} speech segments in {} (redemption_time={}ms)",
        speech_segments.len(), what, VAD_REDEMPTION_TIME_MS
    );

    // Split very long segments at silence boundaries for better transcription quality.
    // Hard cuts at arbitrary sample positions lose words at boundaries. Instead, scan
    // for the lowest-energy window near the target split point and cut there.
    const MAX_SEGMENT_SAMPLES: usize = 25 * 16000; // 25 seconds at 16kHz
    let mut processable_segments: Vec<crate::audio::vad::SpeechSegment> = Vec::new();
    for segment in &speech_segments {
        if segment.samples.len() > MAX_SEGMENT_SAMPLES {
            processable_segments.extend(split_segment_at_silence(segment, MAX_SEGMENT_SAMPLES));
        } else {
            processable_segments.push(segment.clone());
        }
    }

    let processable_count = processable_segments.len();
    let mut transcripts: Vec<SpeakerTranscript> = Vec::new();
    for (i, segment) in processable_segments.iter().enumerate() {
        check_cancelled()?;

        let segment_duration_sec = (segment.end_timestamp_ms - segment.start_timestamp_ms) / 1000.0;
        emit_progress(
            app,
            meeting_id,
            "transcribing",
            span.at(15.0 + (i as f32 / processable_count as f32) * 85.0),
            &format!("Transcribing {}: segment {} of {} ({:.1}s)...", what, i + 1, processable_count, segment_duration_sec),
        );

        // Skip very short segments (< 100ms of audio = 1600 samples at 16kHz)
        if segment.samples.len() < 1600 {
            continue;
        }

        let text = match engine {
            BatchEngine::Parakeet(engine) => engine
                .transcribe_audio(segment.samples.clone())
                .await
                .map_err(|e| anyhow!("Parakeet transcription failed on segment {}: {}", i, e))?,
            BatchEngine::Whisper(engine) => {
                engine
                    .transcribe_audio_with_confidence(segment.samples.clone(), language.clone())
                    .await
                    .map_err(|e| anyhow!("Whisper transcription failed on segment {}: {}", i, e))?
                    .0
            }
        };

        if !text.trim().is_empty() {
            transcripts.push((text, segment.start_timestamp_ms, segment.end_timestamp_ms, label(&segment.samples)));
        }
    }

    info!("Transcribed {} of {} segments from {}", transcripts.len(), processable_count, what);
    Ok((transcripts, duration_seconds))
}

/// A new speaker must share at least this much speech time with an old one to inherit its name.
const MIN_SPEAKER_OVERLAP_SECS: f64 = 3.0;
/// Share of both speakers' overlapping time the match must hold, so a voice that was split in two
/// or two voices that were merged into one don't inherit a name that may belong to someone else.
const MIN_SPEAKER_OVERLAP_SHARE: f64 = 0.6;

/// Pairs each new remote speaker with the old one whose turns it clearly took over, comparing who
/// spoke when in both transcripts. Turns are (speaker, start, end) in seconds; returns new -> old.
fn match_speakers_by_overlap(old: &[(String, f64, f64)], new: &[(String, f64, f64)]) -> HashMap<String, String> {
    let mut overlap: HashMap<(&str, &str), f64> = HashMap::new();
    for (new_key, new_start, new_end) in new.iter().filter(|(key, _, _)| key.starts_with("system-")) {
        for (old_key, old_start, old_end) in old.iter().filter(|(key, _, _)| key.starts_with("system-")) {
            let shared = new_end.min(*old_end) - new_start.max(*old_start);
            if shared > 0.0 {
                *overlap.entry((new_key.as_str(), old_key.as_str())).or_default() += shared;
            }
        }
    }

    let mut total_new: HashMap<&str, f64> = HashMap::new();
    let mut total_old: HashMap<&str, f64> = HashMap::new();
    for (&(new_key, old_key), &secs) in &overlap {
        *total_new.entry(new_key).or_default() += secs;
        *total_old.entry(old_key).or_default() += secs;
    }

    overlap
        .iter()
        .filter(|(&(new_key, old_key), &secs)| {
            secs >= MIN_SPEAKER_OVERLAP_SECS
                && secs / total_new[new_key] >= MIN_SPEAKER_OVERLAP_SHARE
                && secs / total_old[old_key] >= MIN_SPEAKER_OVERLAP_SHARE
        })
        .map(|(&(new_key, old_key), _)| (new_key.to_string(), old_key.to_string()))
        .collect()
}

/// Internal function to run retranscription
async fn run_retranscription<R: Runtime>(
    app: AppHandle<R>,
    meeting_id: String,
    meeting_folder_path: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<RetranscriptionResult> {
    let folder_path = PathBuf::from(&meeting_folder_path);
    let audio_path = find_audio_file(&folder_path)?;
    // Meetings recorded with separate tracks keep who said what; older ones re-transcribe the mix
    let tracks = super::track_recorder::existing_tracks(&folder_path);

    let use_parakeet = provider.as_deref() == Some("parakeet");

    info!(
        "Starting retranscription for meeting {} with language {:?}, model {:?}, provider {:?}, separate tracks: {}",
        meeting_id, language, model, provider, tracks.is_some()
    );

    emit_progress(&app, &meeting_id, "transcribing", 2, "Loading transcription engine...");
    let engine = if use_parakeet {
        BatchEngine::Parakeet(get_or_init_parakeet(&app, model.as_deref()).await?)
    } else {
        BatchEngine::Whisper(get_or_init_whisper(&app, model.as_deref()).await?)
    };

    let sources: Vec<(PathBuf, AudioSource)> = match &tracks {
        Some((mic, system)) => vec![(mic.clone(), AudioSource::Mic), (system.clone(), AudioSource::System)],
        None => vec![(audio_path.clone(), AudioSource::Mixed)],
    };
    let span_len = 75 / sources.len() as u32;

    let mut diarizer = if tracks.is_some() { super::diarization::RecordingDiarizer::load() } else { None };
    let mut all_transcripts: Vec<SpeakerTranscript> = Vec::new();
    let mut duration_seconds = 0.0f64;

    for (index, (path, source)) in sources.iter().enumerate() {
        let span = ProgressSpan { start: 5 + index as u32 * span_len, len: span_len };
        let (transcripts, duration) = transcribe_source(
            &app,
            &meeting_id,
            path,
            *source,
            &engine,
            &language,
            span,
            |samples| match source {
                AudioSource::Mixed => None,
                AudioSource::Mic => Some("mic".to_string()),
                AudioSource::System => Some(match diarizer.as_mut() {
                    Some(diarizer) => diarizer.speaker_key(samples),
                    None => "system".to_string(),
                }),
            },
        )
        .await?;
        duration_seconds = duration_seconds.max(duration);
        all_transcripts.extend(transcripts);
    }

    if all_transcripts.is_empty() {
        warn!("No speech detected in audio");
        return Err(anyhow!("No speech detected in audio file"));
    }
    if let Some(voices) = diarizer.map(super::diarization::RecordingDiarizer::finish) {
        let remote_invitees = match app.try_state::<AppState>() {
            Some(state) => match MeetingsRepository::get_meeting_metadata(state.db_manager.pool(), &meeting_id).await {
                Ok(Some(meeting)) => {
                    super::diarization::remote_invitees(&app, meeting.created_at.0.timestamp() as f64, duration_seconds).await
                }
                _ => None,
            },
            None => None,
        };
        let merges = voices.merges(remote_invitees);
        for (_, _, _, speaker) in all_transcripts.iter_mut() {
            if let Some(merged) = speaker.as_ref().and_then(|key| merges.get(key)) {
                *speaker = Some(merged.clone());
            }
        }
    }
    // Both tracks share one timeline; interleave them back into conversation order
    all_transcripts.sort_by(|a, b| a.1.total_cmp(&b.1));

    check_cancelled()?;
    emit_progress(&app, &meeting_id, "saving", 80, "Saving transcripts...");

    // Create transcript segments with proper timestamps from VAD
    let timed: Vec<(String, f64, f64)> = all_transcripts.iter().map(|(text, s, e, _)| (text.clone(), *s, *e)).collect();
    let mut segments = create_transcript_segments(&timed);
    for (segment, (_, _, _, speaker)) in segments.iter_mut().zip(&all_transcripts) {
        segment.speaker = speaker.clone();
    }

    // Save to database
    let app_state = app
        .try_state::<AppState>()
        .ok_or_else(|| anyhow!("App state not available"))?;

    // Wrap delete+insert+update in a transaction to prevent data loss
    let pool = app_state.db_manager.pool();
    let mut conn = pool.acquire().await.map_err(|e| anyhow!("DB error: {}", e))?;
    let mut tx = sqlx::Connection::begin(&mut *conn)
        .await
        .map_err(|e| anyhow!("Failed to start transaction: {}", e))?;

    // Remote voices get renumbered by the new diarization; capture who spoke when, and their names,
    // before the old transcript is replaced so the names can be carried over.
    let (previous_turns, previous_names) = if tracks.is_some() {
        let turns: Vec<(String, f64, f64)> = sqlx::query_as(
            "SELECT speaker, audio_start_time, audio_end_time FROM transcripts
             WHERE meeting_id = ? AND speaker LIKE 'system-%' AND audio_start_time IS NOT NULL AND audio_end_time IS NOT NULL",
        )
        .bind(&meeting_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| anyhow!("Failed to read previous speakers: {}", e))?;
        let names: HashMap<String, String> =
            sqlx::query_as::<_, (String, String)>("SELECT speaker_key, name FROM meeting_speakers WHERE meeting_id = ?")
                .bind(&meeting_id)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| anyhow!("Failed to read previous speaker names: {}", e))?
                .into_iter()
                .collect();
        (turns, names)
    } else {
        (Vec::new(), HashMap::new())
    };

    sqlx::query("DELETE FROM transcripts WHERE meeting_id = ?")
        .bind(&meeting_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("Failed to delete existing transcripts: {}", e))?;

    for segment in &segments {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&segment.id)
        .bind(&meeting_id)
        .bind(&segment.text)
        .bind(&segment.timestamp)
        .bind(segment.audio_start_time)
        .bind(segment.audio_end_time)
        .bind(segment.duration)
        .bind(&segment.speaker)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("Failed to insert transcript: {}", e))?;
    }

    if tracks.is_some() {
        // Names belong to the old numbering. The local user ('mic') is the same person in both runs;
        // remote names move to whichever new speaker clearly took over the old one's turns.
        sqlx::query("DELETE FROM meeting_speakers WHERE meeting_id = ? AND speaker_key != 'mic'")
            .bind(&meeting_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow!("Failed to reset speaker names: {}", e))?;

        let new_turns: Vec<(String, f64, f64)> = segments
            .iter()
            .filter_map(|s| Some((s.speaker.clone()?, s.audio_start_time?, s.audio_end_time?)))
            .collect();
        for (new_key, old_key) in match_speakers_by_overlap(&previous_turns, &new_turns) {
            let Some(name) = previous_names.get(&old_key) else { continue };
            sqlx::query("INSERT INTO meeting_speakers (meeting_id, speaker_key, name) VALUES (?, ?, ?)")
                .bind(&meeting_id)
                .bind(&new_key)
                .bind(name)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow!("Failed to carry over speaker name: {}", e))?;
        }
    }

    sqlx::query(
        "INSERT INTO meeting_retranscriptions (meeting_id, completed_at, model) VALUES (?, ?, ?)
         ON CONFLICT(meeting_id) DO UPDATE SET completed_at = excluded.completed_at, model = excluded.model",
    )
    .bind(&meeting_id)
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(&model)
    .execute(&mut *tx)
    .await
    .map_err(|e| anyhow!("Failed to record the re-transcription: {}", e))?;

    tx.commit().await
        .map_err(|e| anyhow!("Failed to commit transaction: {}", e))?;

    info!(
        "Updated {} transcripts for meeting {} in transaction",
        segments.len(),
        meeting_id
    );

    if tracks.is_some() {
        crate::summary::speaker_names::auto_apply_in_background(&app, pool.clone(), meeting_id.clone());
    }

    // Write updated transcripts.json and metadata.json to the meeting folder
    emit_progress(&app, &meeting_id, "saving", 90, "Writing transcript files...");

    if let Err(e) = write_transcripts_json(&folder_path, &segments) {
        warn!("Failed to write transcripts.json: {}", e);
    }

    // Find audio filename for metadata
    let audio_filename = audio_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.mp4")
        .to_string();

    if let Err(e) = write_retranscription_metadata(
        &folder_path,
        &meeting_id,
        duration_seconds,
        &audio_filename,
    ) {
        warn!("Failed to update metadata.json: {}", e);
    }

    emit_progress(&app, &meeting_id, "complete", 100, "Retranscription complete");

    Ok(RetranscriptionResult {
        meeting_id,
        segments_count: segments.len(),
        duration_seconds,
        language,
    })
}

/// Emit progress event
fn emit_progress<R: Runtime>(
    app: &AppHandle<R>,
    meeting_id: &str,
    stage: &str,
    progress: u32,
    message: &str,
) {
    let _ = app.emit(
        "retranscription-progress",
        RetranscriptionProgress {
            meeting_id: meeting_id.to_string(),
            stage: stage.to_string(),
            progress_percentage: progress,
            message: message.to_string(),
        },
    );
}

/// Get or initialize the Whisper engine, auto-loading the model if needed
/// If `requested_model` is provided, ensures that specific model is loaded
async fn get_or_init_whisper<R: Runtime>(
    app: &AppHandle<R>,
    requested_model: Option<&str>,
) -> Result<Arc<WhisperEngine>> {
    use crate::whisper_engine::commands::WHISPER_ENGINE;

    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    };

    match engine {
        Some(e) => {
            // Determine which model to use
            let target_model = match requested_model {
                Some(model) => model.to_string(),
                None => get_configured_whisper_model(app).await?,
            };

            // Check if the correct model is already loaded
            let current_model = e.get_current_model().await;
            let needs_load = match &current_model {
                Some(loaded) => loaded != &target_model,
                None => true,
            };

            if needs_load {
                info!(
                    "Loading Whisper model '{}' (current: {:?})",
                    target_model, current_model
                );

                // Discover available models first (populates the internal cache)
                info!("Discovering available Whisper models...");
                if let Err(discover_err) = e.discover_models().await {
                    warn!("Error during model discovery (continuing anyway): {}", discover_err);
                }

                match e.load_model(&target_model).await {
                    Ok(_) => {
                        info!("Whisper model '{}' loaded successfully", target_model);
                        Ok(e)
                    }
                    Err(load_err) => {
                        error!("Failed to load Whisper model '{}': {}", target_model, load_err);
                        Err(anyhow!("Failed to load Whisper model '{}': {}", target_model, load_err))
                    }
                }
            } else {
                info!("Whisper model '{}' already loaded", target_model);
                Ok(e)
            }
        }
        None => Err(anyhow!("Whisper engine not initialized")),
    }
}

/// Get the configured Whisper model name from the database
async fn get_configured_whisper_model<R: Runtime>(app: &AppHandle<R>) -> Result<String> {
    debug!("Getting configured Whisper model from database...");

    let app_state = app
        .try_state::<AppState>()
        .ok_or_else(|| {
            error!("App state not available");
            anyhow!("App state not available")
        })?;

    debug!("Querying transcript_settings table...");

    // Query the transcript settings from the database - get both provider and model
    let result: Option<(String, String)> = sqlx::query_as(
        "SELECT provider, model FROM transcript_settings WHERE id = '1'"
    )
    .fetch_optional(app_state.db_manager.pool())
    .await
    .map_err(|e| {
        error!("Failed to query transcript config: {}", e);
        anyhow!("Failed to query transcript config: {}", e)
    })?;

    match result {
        Some((provider, model)) => {
            info!("Found transcript config: provider={}, model={}", provider, model);

            // Check if provider is Whisper-based
            if provider == "localWhisper" || provider == "whisper" {
                Ok(model)
            } else {
                error!("Retranscription requires Whisper provider, but configured provider is: {}", provider);
                Err(anyhow!("Retranscription requires Whisper. Current provider '{}' does not support retranscription with language selection.", provider))
            }
        },
        None => {
            // Default to configured Whisper model if no config exists
            warn!("No transcript config found, using default model '{}'", DEFAULT_WHISPER_MODEL);
            Ok(DEFAULT_WHISPER_MODEL.to_string())
        }
    }
}

/// Get or initialize the Parakeet engine, auto-loading the model if needed
async fn get_or_init_parakeet<R: Runtime>(
    app: &AppHandle<R>,
    requested_model: Option<&str>,
) -> Result<Arc<ParakeetEngine>> {
    use crate::parakeet_engine::commands::PARAKEET_ENGINE;

    let engine = {
        let guard = PARAKEET_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    };

    match engine {
        Some(e) => {
            // Determine which model to use
            let target_model = match requested_model {
                Some(model) => model.to_string(),
                None => get_configured_parakeet_model(app).await?,
            };

            // Check if the correct model is already loaded
            let current_model = e.get_current_model().await;
            let needs_load = match &current_model {
                Some(loaded) => loaded != &target_model,
                None => true,
            };

            if needs_load {
                info!(
                    "Loading Parakeet model '{}' (current: {:?})",
                    target_model, current_model
                );

                // Discover available models first
                info!("Discovering available Parakeet models...");
                if let Err(discover_err) = e.discover_models().await {
                    warn!("Error during Parakeet model discovery (continuing anyway): {}", discover_err);
                }

                match e.load_model(&target_model).await {
                    Ok(_) => {
                        info!("Parakeet model '{}' loaded successfully", target_model);
                        Ok(e)
                    }
                    Err(load_err) => {
                        error!("Failed to load Parakeet model '{}': {}", target_model, load_err);
                        Err(anyhow!("Failed to load Parakeet model '{}': {}", target_model, load_err))
                    }
                }
            } else {
                info!("Parakeet model '{}' already loaded", target_model);
                Ok(e)
            }
        }
        None => Err(anyhow!("Parakeet engine not initialized")),
    }
}

/// Get the configured Parakeet model name from the database
async fn get_configured_parakeet_model<R: Runtime>(app: &AppHandle<R>) -> Result<String> {
    debug!("Getting configured Parakeet model from database...");

    let app_state = app
        .try_state::<AppState>()
        .ok_or_else(|| {
            error!("App state not available");
            anyhow!("App state not available")
        })?;

    // Query the transcript settings from the database
    let result: Option<(String, String)> = sqlx::query_as(
        "SELECT provider, model FROM transcript_settings WHERE id = '1'"
    )
    .fetch_optional(app_state.db_manager.pool())
    .await
    .map_err(|e| {
        error!("Failed to query transcript config: {}", e);
        anyhow!("Failed to query transcript config: {}", e)
    })?;

    match result {
        Some((provider, model)) => {
            info!("Found transcript config: provider={}, model={}", provider, model);

            if provider == "parakeet" {
                Ok(model)
            } else {
                // Default to configured Parakeet model
                warn!("Configured provider is not Parakeet, using default model");
                Ok(DEFAULT_PARAKEET_MODEL.to_string())
            }
        },
        None => {
            // Default to configured Parakeet model if no config exists
            warn!("No transcript config found, using default Parakeet model");
            Ok(DEFAULT_PARAKEET_MODEL.to_string())
        }
    }
}

/// Write or update metadata.json for retranscription (preserves existing fields, adds retranscribed_at)
fn write_retranscription_metadata(
    folder: &Path,
    meeting_id: &str,
    duration_seconds: f64,
    audio_filename: &str,
) -> Result<()> {
    let metadata_path = folder.join("metadata.json");
    let temp_path = folder.join(".metadata.json.tmp");
    let now = chrono::Utc::now().to_rfc3339();

    // Try to read existing metadata and update it
    let json = if metadata_path.exists() {
        let existing = std::fs::read_to_string(&metadata_path)?;
        let mut value: serde_json::Value = serde_json::from_str(&existing)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("duration_seconds".to_string(), serde_json::json!(duration_seconds));
            obj.insert("retranscribed_at".to_string(), serde_json::json!(now));
            obj.insert("status".to_string(), serde_json::json!("completed"));
            obj.insert("transcript_file".to_string(), serde_json::json!("transcripts.json"));
            obj.remove("detected_summary_language");
        }
        value
    } else {
        serde_json::json!({
            "version": "1.0",
            "meeting_id": meeting_id,
            "created_at": now,
            "completed_at": now,
            "retranscribed_at": now,
            "duration_seconds": duration_seconds,
            "audio_file": audio_filename,
            "transcript_file": "transcripts.json",
            "status": "completed",
            "source": "retranscription"
        })
    };

    let json_string = serde_json::to_string_pretty(&json)?;
    std::fs::write(&temp_path, &json_string)?;
    std::fs::rename(&temp_path, &metadata_path)?;

    info!("Wrote metadata.json to {}", metadata_path.display());
    Ok(())
}

// Tauri commands

/// Response when retranscription is started
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetranscriptionStarted {
    pub meeting_id: String,
    pub message: String,
}

// Start retranscription (Beta gated using configContext.betaFeatures)
#[tauri::command]
pub async fn start_retranscription_command<R: Runtime>(
    app: AppHandle<R>,
    meeting_id: String,
    meeting_folder_path: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<RetranscriptionStarted, String> {

    // Check if retranscription is already in progress (guard will be acquired in start_retranscription)
    if RETRANSCRIPTION_IN_PROGRESS.load(Ordering::SeqCst) {
        return Err("Retranscription already in progress".to_string());
    }

    // Clone values for the spawned task
    let meeting_id_clone = meeting_id.clone();

    // Spawn the retranscription in a background task
    tauri::async_runtime::spawn(async move {
        let result = start_retranscription(
            app,
            meeting_id_clone,
            meeting_folder_path,
            language,
            model,
            provider,
        )
        .await;

        // Errors are already emitted as events in start_retranscription
        // so we just log here for debugging
        if let Err(e) = result {
            error!("Retranscription failed: {}", e);
        }
    });

    Ok(RetranscriptionStarted {
        meeting_id,
        message: "Retranscription started".to_string(),
    })
}

#[tauri::command]
pub async fn cancel_retranscription_command() -> Result<(), String> {
    if !is_retranscription_in_progress() {
        return Err("No retranscription in progress".to_string());
    }
    cancel_retranscription();
    Ok(())
}

#[tauri::command]
pub async fn is_retranscription_in_progress_command() -> bool {
    is_retranscription_in_progress()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns(list: &[(&str, f64, f64)]) -> Vec<(String, f64, f64)> {
        list.iter().map(|(k, s, e)| (k.to_string(), *s, *e)).collect()
    }

    #[test]
    fn renumbered_speakers_inherit_from_the_old_speaker_they_replace() {
        let old = turns(&[("system-1", 0.0, 10.0), ("system-3", 10.0, 20.0), ("system-1", 20.0, 30.0)]);
        let new = turns(&[("system-2", 0.0, 10.0), ("system-1", 10.0, 20.0), ("system-2", 20.0, 31.0)]);
        let matched = match_speakers_by_overlap(&old, &new);
        assert_eq!(matched.len(), 2);
        assert_eq!(matched["system-2"], "system-1");
        assert_eq!(matched["system-1"], "system-3");
    }

    #[test]
    fn a_voice_split_in_two_does_not_pass_its_name_on() {
        let old = turns(&[("system-1", 0.0, 20.0)]);
        let new = turns(&[("system-1", 0.0, 10.0), ("system-2", 10.0, 20.0)]);
        assert!(match_speakers_by_overlap(&old, &new).is_empty());
    }

    #[test]
    fn two_voices_merged_into_one_do_not_pass_a_name_on() {
        let old = turns(&[("system-1", 0.0, 10.0), ("system-2", 10.0, 20.0)]);
        let new = turns(&[("system-1", 0.0, 20.0)]);
        assert!(match_speakers_by_overlap(&old, &new).is_empty());
    }

    #[test]
    fn brief_overlaps_and_non_remote_speakers_are_ignored() {
        let old = turns(&[("system-1", 0.0, 2.0), ("mic", 2.0, 30.0)]);
        let new = turns(&[("system-1", 0.0, 2.0), ("mic", 2.0, 30.0)]);
        assert!(match_speakers_by_overlap(&old, &new).is_empty());
    }

    #[test]
    fn test_create_transcript_segments_empty() {
        let transcripts: Vec<(String, f64, f64)> = vec![];
        let segments = create_transcript_segments(&transcripts);
        assert!(segments.is_empty());
    }

    #[test]
    fn test_create_transcript_segments_single() {
        let transcripts = vec![
            ("Hello world".to_string(), 0.0, 1500.0), // 0-1.5 seconds
        ];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Hello world");
        assert_eq!(segments[0].audio_start_time, Some(0.0));
        assert_eq!(segments[0].audio_end_time, Some(1.5));
        assert_eq!(segments[0].duration, Some(1.5));
    }

    #[test]
    fn test_create_transcript_segments_multiple() {
        let transcripts = vec![
            ("First segment".to_string(), 0.0, 2000.0),      // 0-2 seconds
            ("Second segment".to_string(), 3000.0, 5000.0),  // 3-5 seconds
            ("Third segment".to_string(), 6500.0, 8000.0),   // 6.5-8 seconds
        ];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 3);

        // First segment
        assert_eq!(segments[0].text, "First segment");
        assert_eq!(segments[0].audio_start_time, Some(0.0));
        assert_eq!(segments[0].audio_end_time, Some(2.0));
        assert_eq!(segments[0].duration, Some(2.0));

        // Second segment
        assert_eq!(segments[1].text, "Second segment");
        assert_eq!(segments[1].audio_start_time, Some(3.0));
        assert_eq!(segments[1].audio_end_time, Some(5.0));
        assert_eq!(segments[1].duration, Some(2.0));

        // Third segment
        assert_eq!(segments[2].text, "Third segment");
        assert_eq!(segments[2].audio_start_time, Some(6.5));
        assert_eq!(segments[2].audio_end_time, Some(8.0));
        assert_eq!(segments[2].duration, Some(1.5));
    }

    #[test]
    fn test_create_transcript_segments_trims_whitespace() {
        let transcripts = vec![
            ("  Hello with spaces  ".to_string(), 0.0, 1000.0),
        ];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Hello with spaces");
    }

    #[test]
    fn test_create_transcript_segments_generates_unique_ids() {
        let transcripts = vec![
            ("Segment one".to_string(), 0.0, 1000.0),
            ("Segment two".to_string(), 1000.0, 2000.0),
        ];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 2);
        assert_ne!(segments[0].id, segments[1].id);
        assert!(segments[0].id.starts_with("transcript-"));
        assert!(segments[1].id.starts_with("transcript-"));
    }

    #[test]
    fn test_cancellation_flag() {
        // Reset flag to known state
        RETRANSCRIPTION_CANCELLED.store(false, Ordering::SeqCst);
        RETRANSCRIPTION_IN_PROGRESS.store(false, Ordering::SeqCst);

        assert!(!is_retranscription_in_progress());

        // Test cancellation
        cancel_retranscription();
        assert!(RETRANSCRIPTION_CANCELLED.load(Ordering::SeqCst));

        // Reset for other tests
        RETRANSCRIPTION_CANCELLED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn test_vad_redemption_time_constant() {
        // Batch processing uses 2000ms to bridge natural pauses in full-file VAD
        assert_eq!(VAD_REDEMPTION_TIME_MS, 2000);
    }

    #[test]
    fn test_find_audio_file_common_candidates() {
        let dir = tempfile::tempdir().unwrap();

        // No audio file → error
        assert!(find_audio_file(dir.path()).is_err());

        // Create audio.mp4 — should be found first
        std::fs::write(dir.path().join("audio.mp4"), b"fake").unwrap();
        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "audio.mp4");
    }

    #[test]
    fn test_find_audio_file_non_mp4_extensions() {
        let dir = tempfile::tempdir().unwrap();

        // Create audio.wav (imported as .wav, not .mp4)
        std::fs::write(dir.path().join("audio.wav"), b"fake").unwrap();
        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "audio.wav");
    }

    #[test]
    fn test_find_audio_file_fallback_scan() {
        let dir = tempfile::tempdir().unwrap();

        // Create a file with an audio extension but non-standard name
        std::fs::write(dir.path().join("my_recording.flac"), b"fake").unwrap();
        // Also add a non-audio file that should be ignored
        std::fs::write(dir.path().join("notes.txt"), b"text").unwrap();

        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "my_recording.flac");
    }

    #[test]
    fn test_find_audio_file_priority_order() {
        let dir = tempfile::tempdir().unwrap();

        // Create both audio.m4a and audio.mp4 — mp4 should win (listed first in candidates)
        std::fs::write(dir.path().join("audio.m4a"), b"fake").unwrap();
        std::fs::write(dir.path().join("audio.mp4"), b"fake").unwrap();
        let found = find_audio_file(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "audio.mp4");
    }

    #[test]
    fn test_find_audio_file_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_audio_file(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No audio file found"));
    }

    #[test]
    fn test_find_audio_file_nonexistent_folder() {
        let result = find_audio_file(Path::new("/nonexistent/path/12345"));
        assert!(result.is_err());
    }

    #[test]
    fn test_audio_extensions_constant() {
        // Verify all expected formats are covered
        assert!(AUDIO_EXTENSIONS.contains(&"mp4"));
        assert!(AUDIO_EXTENSIONS.contains(&"m4a"));
        assert!(AUDIO_EXTENSIONS.contains(&"wav"));
        assert!(AUDIO_EXTENSIONS.contains(&"mp3"));
        assert!(AUDIO_EXTENSIONS.contains(&"flac"));
        assert!(AUDIO_EXTENSIONS.contains(&"ogg"));
        assert!(AUDIO_EXTENSIONS.contains(&"aac"));
        // FFmpeg-backed formats
        assert!(AUDIO_EXTENSIONS.contains(&"mkv"));
        assert!(AUDIO_EXTENSIONS.contains(&"webm"));
        assert!(AUDIO_EXTENSIONS.contains(&"wma"));
        // Non-audio formats
        assert!(!AUDIO_EXTENSIONS.contains(&"txt"));
        assert!(!AUDIO_EXTENSIONS.contains(&"pdf"));
    }

    #[test]
    fn retranscription_metadata_replaces_stale_duration_without_losing_existing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let metadata_path = dir.path().join("metadata.json");
        std::fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "meeting_id": "existing-meeting",
                "duration_seconds": 2523.2,
                "audio_file": "recording.m4a",
                "summary_language": "fr",
                "detected_summary_language": "en",
                "custom_field": "preserve me"
            }))
            .unwrap(),
        )
        .unwrap();

        write_retranscription_metadata(
            dir.path(),
            "ignored-for-existing-metadata",
            5046.4,
            "recording.m4a",
        )
        .unwrap();

        let metadata: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(metadata_path).unwrap()).unwrap();
        assert_eq!(metadata["duration_seconds"], 5046.4);
        assert_eq!(metadata["meeting_id"], "existing-meeting");
        assert_eq!(metadata["audio_file"], "recording.m4a");
        assert_eq!(metadata["summary_language"], "fr");
        assert_eq!(metadata["custom_field"], "preserve me");
        assert!(metadata.get("detected_summary_language").is_none());
    }
}
