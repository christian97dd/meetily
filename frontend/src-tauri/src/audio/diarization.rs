//! Tells remote speakers apart. Every system-audio speech segment gets a voice embedding
//! (WeSpeaker CAM++, local ONNX) and is matched against the voices heard so far in the recording,
//! so "Others" becomes Speaker 1, 2, 3...
//!
//! Each VAD segment is treated as a single speaker: they are short speech bursts, and overlaps are rare
//! enough in calls that per-segment matching beats running a separate segmentation model.
//!
//! Matching happens as the audio arrives, so a person whose audio changes mid-call (codec, mic,
//! distance) can be split into two voices. Once the whole recording was heard, voices that turn out
//! to be one person are merged.

use std::collections::HashMap;
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
/// Averaged over a whole voice, the same person split in two scored 0.46-0.60 on real calls and
/// different people up to ~0.4. Above this, two voices are one person.
const SAME_VOICE_SIMILARITY: f32 = 0.5;
/// Pairs between this and `SAME_VOICE_SIMILARITY` are doubtful: merged only while there are more
/// voices than remote invitees, so someone who joined uninvited still keeps their own voice.
const DOUBTFUL_VOICE_SIMILARITY: f32 = 0.4;

/// Known voices of one recording, each kept as the running mean of its (unit-length) embeddings.
/// `members` keeps the embeddings each voice was built from, for the final merge.
struct SpeakerRegistry {
    centroids: Vec<(Vec<f32>, u32)>,
    members: Vec<Vec<Vec<f32>>>,
}

impl SpeakerRegistry {
    fn new() -> Self {
        Self { centroids: Vec::new(), members: Vec::new() }
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
                self.centroids.push((embedding.clone(), 1));
                self.members.push(vec![embedding]);
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
        self.members[index].push(embedding);
        Some(index + 1)
    }
}

/// New 1-based id for each voice (by index) after merging the ones that are the same person.
/// Merges the most similar pair first; ids stay in order of first appearance.
fn merge_plan(voices: &[Vec<Vec<f32>>], remote_invitees: Option<usize>) -> Vec<usize> {
    let sum = |embeddings: &[Vec<f32>]| {
        let mut total = vec![0.0f32; embeddings.first().map_or(0, Vec::len)];
        for embedding in embeddings {
            for (t, e) in total.iter_mut().zip(embedding) {
                *t += e;
            }
        }
        total
    };
    // Each group: the voices it holds and the sum of all their embeddings
    let mut groups: Vec<(Vec<usize>, Vec<f32>)> =
        voices.iter().enumerate().map(|(index, embeddings)| (vec![index], sum(embeddings))).collect();

    loop {
        let mut best: Option<(usize, usize, f32)> = None;
        for a in 0..groups.len() {
            let Some(mean_a) = normalized(&groups[a].1) else { continue };
            for b in a + 1..groups.len() {
                let similarity = cosine(&mean_a, &groups[b].1);
                if best.map_or(true, |(_, _, s)| similarity > s) {
                    best = Some((a, b, similarity));
                }
            }
        }
        let Some((a, b, similarity)) = best else { break };
        let too_many = remote_invitees.is_some_and(|invitees| groups.len() > invitees);
        if similarity < SAME_VOICE_SIMILARITY && !(too_many && similarity >= DOUBTFUL_VOICE_SIMILARITY) {
            break;
        }
        let (voices_b, sum_b) = groups.remove(b);
        groups[a].0.extend(voices_b);
        for (t, e) in groups[a].1.iter_mut().zip(&sum_b) {
            *t += e;
        }
    }

    let mut new_ids = vec![0; voices.len()];
    for (group_index, (members, _)) in groups.iter().enumerate() {
        for &voice in members {
            new_ids[voice] = group_index + 1;
        }
    }
    new_ids
}

/// All the voices of a finished recording, ready to be merged.
pub struct FinishedVoices(SpeakerRegistry);

impl FinishedVoices {
    /// Old speaker key -> merged key, only for the keys that change. `remote_invitees` comes from
    /// the calendar and only settles doubtful pairs.
    pub fn merges(&self, remote_invitees: Option<usize>) -> HashMap<String, String> {
        let new_ids = merge_plan(&self.0.members, remote_invitees);
        let merges: HashMap<String, String> = new_ids
            .iter()
            .enumerate()
            .filter(|(index, new_id)| index + 1 != **new_id)
            .map(|(index, new_id)| (speaker_key(Some(index + 1)), speaker_key(Some(*new_id))))
            .collect();
        let remaining = new_ids.iter().max().copied().unwrap_or(0);
        if remaining < new_ids.len() {
            info!("Diarization: merged {} remote voices into {}", new_ids.len(), remaining);
        }
        merges
    }
}

/// Remote people invited to the calendar event around a recording, when calendar naming is on.
pub async fn remote_invitees<R: Runtime>(app: &AppHandle<R>, end_unix: f64, duration_secs: f64) -> Option<usize> {
    let slack = crate::summary::speaker_names::CALENDAR_SLACK_SECS;
    let attendees = crate::calendar::attendees_between(app, end_unix - duration_secs - slack, end_unix + slack).await?;
    (!attendees.others.is_empty()).then_some(attendees.others.len())
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
/// Voices of the last live recording, kept from its end until its transcript is saved.
static FINISHED_SESSION: Mutex<Option<FinishedVoices>> = Mutex::new(None);

/// Voices of one recording. The live pipeline keeps one in `SESSION`; batch re-transcription creates its own.
pub struct RecordingDiarizer {
    extractor: EmbeddingExtractor,
    speakers: SpeakerRegistry,
    last_speaker: Option<usize>,
}

impl RecordingDiarizer {
    /// None when the model isn't downloaded yet or fails to load.
    pub fn load() -> Option<Self> {
        let path = MODEL_PATH.get().filter(|path| path.exists())?;
        match EmbeddingExtractor::new(path) {
            Ok(extractor) => Some(Self { extractor, speakers: SpeakerRegistry::new(), last_speaker: None }),
            Err(e) => {
                warn!("Diarization: failed to load model: {}", e);
                None
            }
        }
    }

    /// Speaker key for a system-audio segment: "system-N", or "system" when voices can't be told apart.
    pub fn speaker_key(&mut self, samples: &[f32]) -> String {
        speaker_key(self.speaker_for(samples))
    }

    pub fn finish(self) -> FinishedVoices {
        FinishedVoices(self.speakers)
    }

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
    let diarizer = RecordingDiarizer::load();
    info!("Diarization: {}", if diarizer.is_some() { "enabled for this recording" } else { "unavailable" });
    if let Ok(mut finished) = FINISHED_SESSION.lock() {
        *finished = None;
    }
    if let Ok(mut session) = SESSION.lock() {
        *session = diarizer;
    }
}

pub fn end_session() {
    let diarizer = SESSION.lock().ok().and_then(|mut session| session.take());
    if let Ok(mut finished) = FINISHED_SESSION.lock() {
        *finished = diarizer.map(RecordingDiarizer::finish);
    }
}

/// Voices of the live recording that just ended; taken once, by whoever saves its transcript.
pub fn take_finished_session() -> Option<FinishedVoices> {
    FINISHED_SESSION.lock().ok()?.take()
}

/// Speaker key for a system-audio segment: "system-N", or "system" when voices can't be told apart.
pub fn system_speaker_key(samples: &[f32]) -> String {
    speaker_key(SESSION.lock().ok().and_then(|mut session| session.as_mut()?.speaker_for(samples)))
}

fn speaker_key(speaker: Option<usize>) -> String {
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

    /// Unit vector at `similarity` cosine from [1, 0, 0].
    fn at(similarity: f32) -> Vec<f32> {
        vec![similarity, (1.0 - similarity * similarity).sqrt(), 0.0]
    }

    #[test]
    fn one_person_split_in_two_voices_is_merged_back() {
        let voices = vec![vec![at(1.0), at(0.95)], vec![at(0.6), at(0.65)]];
        assert_eq!(merge_plan(&voices, None), vec![1, 1]);
    }

    #[test]
    fn different_people_are_never_merged_even_above_the_invitee_count() {
        let voices = vec![vec![at(1.0)], vec![at(0.2)], vec![vec![0.0, 0.0, 1.0]]];
        assert_eq!(merge_plan(&voices, None), vec![1, 2, 3]);
        assert_eq!(merge_plan(&voices, Some(1)), vec![1, 2, 3]);
    }

    #[test]
    fn doubtful_voices_are_merged_only_while_there_are_more_than_invitees() {
        let voices = vec![vec![at(1.0)], vec![at(0.45)]];
        assert_eq!(merge_plan(&voices, None), vec![1, 2]);
        assert_eq!(merge_plan(&voices, Some(2)), vec![1, 2]);
        assert_eq!(merge_plan(&voices, Some(1)), vec![1, 1]);
    }

    #[test]
    fn merged_ids_follow_the_order_voices_first_appeared() {
        let voices = vec![vec![at(1.0)], vec![vec![0.0, 0.0, 1.0]], vec![at(0.9)], vec![vec![0.0, 0.1, 1.0]]];
        assert_eq!(merge_plan(&voices, None), vec![1, 2, 1, 2]);
    }

    #[test]
    fn merges_only_list_the_speaker_keys_that_change() {
        let mut registry = SpeakerRegistry::new();
        registry.assign(&at(1.0), true);
        registry.assign(&vec![0.0, 0.0, 1.0], true);
        registry.members.push(vec![at(0.9)]);
        let merges = FinishedVoices(registry).merges(None);
        assert_eq!(merges, HashMap::from([("system-3".to_string(), "system-1".to_string())]));
    }

    #[test]
    fn every_embedding_a_voice_is_built_from_is_kept() {
        let mut registry = SpeakerRegistry::new();
        registry.assign(&voice(0.0, 0.0), true);
        registry.assign(&voice(0.0, 0.3), true);
        registry.assign(&[1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0], true);
        assert_eq!(registry.members.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1]);
    }
}
