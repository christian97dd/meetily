//! Local HTTP API so other tools on this machine (an MCP server, scripts) can drive meetily:
//! list meetings, summarize with the configured local model, export, control recording.
//!
//! Off by default. Listens on 127.0.0.1 only and every request needs the bearer token stored in
//! `local-api.json` in the app data dir, which is also where clients discover the port. Endpoints
//! that return meeting content (transcripts, summaries, search) additionally need `allow_content`,
//! so a client can be given control without reading what was said.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_store::StoreExt;

use crate::database::models::Transcript;
use crate::database::repositories::{
    meeting::MeetingsRepository, setting::SettingsRepository, speaker::SpeakersRepository,
    transcript::TranscriptsRepository,
};
use crate::state::AppState;

const STORE_FILE: &str = "local-api.json";
const STORE_KEY: &str = "settings";
const DEFAULT_PORT: u16 = 47320;
/// A local summary of a long meeting can take a while; stop waiting to write it after this.
const SUMMARY_WAIT: Duration = Duration::from_secs(30 * 60);

static SHUTDOWN: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalApiSettings {
    pub enabled: bool,
    /// Lets clients read transcripts, summaries and search results, not only metadata and actions.
    pub allow_content: bool,
    pub port: u16,
    pub token: String,
}

impl Default for LocalApiSettings {
    fn default() -> Self {
        Self { enabled: false, allow_content: false, port: DEFAULT_PORT, token: String::new() }
    }
}

fn load_settings(app: &AppHandle<Wry>) -> LocalApiSettings {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(STORE_KEY))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn save_settings(app: &AppHandle<Wry>, settings: &LocalApiSettings) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| format!("Failed to access local API store: {}", e))?;
    store.set(STORE_KEY, serde_json::to_value(settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("Failed to save local API settings: {}", e))
}

fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

// ---------- Server lifecycle ----------

#[derive(Clone)]
struct ApiState {
    app: AppHandle<Wry>,
    token: String,
    allow_content: bool,
}

/// Starts the server at app startup when the user enabled it.
pub fn init(app: &AppHandle<Wry>) {
    let settings = load_settings(app);
    if settings.enabled {
        restart(app, &settings);
    }
}

fn stop() {
    if let Some(shutdown) = SHUTDOWN.lock().ok().and_then(|mut s| s.take()) {
        let _ = shutdown.send(());
    }
}

fn restart(app: &AppHandle<Wry>, settings: &LocalApiSettings) {
    stop();
    if !settings.enabled || settings.token.is_empty() {
        return;
    }

    let state = ApiState { app: app.clone(), token: settings.token.clone(), allow_content: settings.allow_content };
    let port = settings.port;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    if let Ok(mut shutdown) = SHUTDOWN.lock() {
        *shutdown = Some(shutdown_tx);
    }

    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(e) => {
                error!("Local API: cannot listen on 127.0.0.1:{}: {}", port, e);
                return;
            }
        };
        info!("Local API listening on http://127.0.0.1:{}", port);
        let served = axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
        if let Err(e) = served {
            error!("Local API stopped with an error: {}", e);
        } else {
            info!("Local API stopped");
        }
    });
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/meetings", get(list_meetings))
        .route("/v1/meetings/{id}", get(get_meeting))
        .route("/v1/meetings/{id}/transcript", get(get_transcript))
        .route("/v1/meetings/{id}/summary", get(get_summary))
        .route("/v1/meetings/{id}/summarize", post(summarize))
        .route("/v1/meetings/{id}/export", post(export))
        .route("/v1/search", get(search))
        .route("/v1/templates", get(list_templates))
        .route("/v1/recording", get(recording_status))
        .route("/v1/recording/start", post(start_recording))
        .route("/v1/recording/stop", post(stop_recording))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

fn token_matches(header_value: Option<&str>, token: &str) -> bool {
    header_value
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| !token.is_empty() && presented.trim() == token)
}

async fn require_token(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    let presented = request.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if token_matches(presented, &state.token) {
        next.run(request).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "Missing or invalid bearer token".into()).into_response()
    }
}

// ---------- Errors ----------

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn require_content(state: &ApiState) -> Result<(), ApiError> {
    if state.allow_content {
        Ok(())
    } else {
        Err(ApiError(StatusCode::FORBIDDEN, "Content access is disabled in meetily Preferences".into()))
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

// ---------- Transcript formatting ----------

/// Same labels as the app (frontend/src/lib/speaker.ts): custom name, else You / Others / Speaker N.
fn speaker_label(speaker: Option<&str>, names: &HashMap<String, String>) -> Option<String> {
    let speaker = speaker?;
    if let Some(name) = names.get(speaker) {
        return Some(name.clone());
    }
    match speaker {
        "mic" => Some("You".into()),
        "system" => Some("Others".into()),
        other => other.strip_prefix("system-").map(|n| format!("Speaker {}", n)),
    }
}

fn format_time(seconds: Option<f64>, fallback: &str) -> String {
    match seconds {
        Some(s) => {
            let total = s.max(0.0) as u64;
            format!("[{:02}:{:02}]", total / 60, total % 60)
        }
        None => fallback.to_string(),
    }
}

fn transcript_lines(transcripts: &[Transcript], names: &HashMap<String, String>) -> Vec<String> {
    transcripts
        .iter()
        .map(|t| {
            let time = format_time(t.audio_start_time, &t.timestamp);
            match speaker_label(t.speaker.as_deref(), names) {
                Some(label) => format!("{} {}: {}", time, label, t.transcript),
                None => format!("{} {}", time, t.transcript),
            }
        })
        .collect()
}

/// Matches the app's Copy/Export output (frontend/src/lib/transcript-document.ts).
fn transcript_document(meeting_id: &str, title: &str, date: &str, lines: &[String], format: &str) -> String {
    if format == "txt" {
        format!("{}\n{}\n\n{}\n", title, date, lines.join("\n"))
    } else {
        let body: Vec<String> = lines.iter().map(|l| format!("{}  ", l)).collect();
        format!("# Transcript of the Meeting: {} - {}\n\n## Date: {}\n\n{}", meeting_id, title, date, body.join("\n"))
    }
}

struct MeetingContent {
    title: String,
    created_at: String,
    transcripts: Vec<Transcript>,
    names: HashMap<String, String>,
}

async fn load_meeting(state: &ApiState, id: &str) -> Result<MeetingContent, ApiError> {
    let app_state = state.app.state::<AppState>();
    let pool = app_state.db_manager.pool();
    let meeting = MeetingsRepository::get_meeting_metadata(pool, id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("Meeting not found: {}", id)))?;
    let (transcripts, _) = MeetingsRepository::get_meeting_transcripts_paginated(pool, id, i64::MAX, 0)
        .await
        .map_err(internal)?;
    let names = SpeakersRepository::get_names(pool, id).await.map_err(internal)?;
    Ok(MeetingContent { title: meeting.title, created_at: meeting.created_at.0.to_rfc3339(), transcripts, names })
}

fn summary_markdown(data: &Option<Value>) -> Option<String> {
    let data = data.as_ref()?;
    match data.get("markdown").and_then(Value::as_str) {
        Some(markdown) => Some(markdown.to_string()),
        None => Some(serde_json::to_string_pretty(data).ok()?),
    }
}

/// Writes only to absolute paths whose folder already exists, so a typo can't scatter files.
fn checked_output_path(path: &str) -> Result<PathBuf, ApiError> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Path must be absolute".into()));
    }
    if !path.parent().is_some_and(|parent| parent.is_dir()) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Destination folder does not exist".into()));
    }
    Ok(path)
}

// ---------- Handlers ----------

async fn status(State(state): State<ApiState>) -> ApiResult {
    Ok(Json(json!({
        "app": "meetily",
        "version": env!("CARGO_PKG_VERSION"),
        "content_access": state.allow_content,
        "recording": crate::audio::recording_commands::get_recording_state().await,
    })))
}

#[derive(Deserialize)]
struct ListQuery {
    /// Case-insensitive match on the title
    q: Option<String>,
    limit: Option<usize>,
}

async fn list_meetings(State(state): State<ApiState>, Query(query): Query<ListQuery>) -> ApiResult {
    let app_state = state.app.state::<AppState>();
    let pool = app_state.db_manager.pool();
    let needle = query.q.map(|q| q.to_lowercase());
    let meetings: Vec<Value> = MeetingsRepository::get_meetings(pool)
        .await
        .map_err(internal)?
        .into_iter()
        .filter(|m| needle.as_ref().map_or(true, |n| m.title.to_lowercase().contains(n)))
        .take(query.limit.unwrap_or(50))
        .map(|m| json!({ "id": m.id, "title": m.title, "created_at": m.created_at.0.to_rfc3339() }))
        .collect();
    Ok(Json(json!({ "meetings": meetings })))
}

async fn get_meeting(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult {
    let meeting = load_meeting(&state, &id).await?;
    let app_state = state.app.state::<AppState>();
    let pool = app_state.db_manager.pool();

    let duration = meeting.transcripts.iter().filter_map(|t| t.audio_end_time).fold(0.0, f64::max);
    let mut speakers: Vec<String> = meeting.transcripts.iter().filter_map(|t| t.speaker.clone()).collect();
    speakers.sort();
    speakers.dedup();
    let speakers: Vec<Value> = speakers
        .iter()
        .map(|key| json!({ "key": key, "label": speaker_label(Some(key), &meeting.names) }))
        .collect();

    let folder = MeetingsRepository::get_meeting_metadata(pool, &id).await.map_err(internal)?.and_then(|m| m.folder_path);
    let has_tracks = folder
        .as_deref()
        .is_some_and(|f| crate::audio::track_recorder::existing_tracks(std::path::Path::new(f)).is_some());
    let summary = crate::summary::commands::api_get_summary(state.app.clone(), state.app.state::<AppState>(), id.clone(), None)
        .await
        .map_err(internal)?;

    Ok(Json(json!({
        "id": id,
        "title": meeting.title,
        "created_at": meeting.created_at,
        "duration_seconds": duration,
        "segments": meeting.transcripts.len(),
        "speakers": speakers,
        "has_separate_tracks": has_tracks,
        "summary_status": summary.status,
    })))
}

#[derive(Deserialize)]
struct TranscriptQuery {
    /// json (default), md or txt
    format: Option<String>,
}

async fn get_transcript(State(state): State<ApiState>, Path(id): Path<String>, Query(query): Query<TranscriptQuery>) -> Result<Response, ApiError> {
    require_content(&state)?;
    let meeting = load_meeting(&state, &id).await?;
    match query.format.as_deref().unwrap_or("json") {
        "json" => {
            let segments: Vec<Value> = meeting
                .transcripts
                .iter()
                .map(|t| json!({
                    "start": t.audio_start_time,
                    "end": t.audio_end_time,
                    "speaker": t.speaker,
                    "label": speaker_label(t.speaker.as_deref(), &meeting.names),
                    "text": t.transcript,
                }))
                .collect();
            Ok(Json(json!({ "id": id, "title": meeting.title, "segments": segments })).into_response())
        }
        format @ ("md" | "txt") => {
            let lines = transcript_lines(&meeting.transcripts, &meeting.names);
            Ok(transcript_document(&id, &meeting.title, &meeting.created_at, &lines, format).into_response())
        }
        other => Err(ApiError(StatusCode::BAD_REQUEST, format!("Unknown format: {}", other))),
    }
}

async fn get_summary(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult {
    let summary = crate::summary::commands::api_get_summary(state.app.clone(), state.app.state::<AppState>(), id.clone(), None)
        .await
        .map_err(internal)?;
    let markdown = if state.allow_content { summary_markdown(&summary.data) } else { None };
    Ok(Json(json!({ "id": id, "status": summary.status, "error": summary.error, "markdown": markdown })))
}

#[derive(Deserialize)]
struct SummarizeBody {
    template_id: String,
    custom_prompt: Option<String>,
    /// Absolute path to write the summary to once done, so the caller never receives the content
    write_to: Option<String>,
}

async fn summarize(State(state): State<ApiState>, Path(id): Path<String>, Json(body): Json<SummarizeBody>) -> ApiResult {
    if !crate::summary::templates::list_template_ids().contains(&body.template_id) {
        return Err(ApiError(StatusCode::BAD_REQUEST, format!("Unknown template: {}", body.template_id)));
    }
    let write_to = body.write_to.as_deref().map(checked_output_path).transpose()?;

    let meeting = load_meeting(&state, &id).await?;
    if meeting.transcripts.is_empty() {
        return Err(ApiError(StatusCode::CONFLICT, "Meeting has no transcript".into()));
    }
    let app_state = state.app.state::<AppState>();
    let config = SettingsRepository::get_model_config(app_state.db_manager.pool())
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "No summary model configured".into()))?;

    // Same text the app sends from the Generate button: timestamped lines with speaker names
    let text = transcript_lines(&meeting.transcripts, &meeting.names).join("\n");
    crate::summary::commands::api_process_transcript(
        state.app.clone(),
        state.app.state::<AppState>(),
        text,
        config.provider,
        config.model,
        Some(id.clone()),
        None,
        None,
        body.custom_prompt,
        Some(body.template_id),
        None,
        None,
    )
    .await
    .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;

    if let Some(path) = write_to.clone() {
        let app = state.app.clone();
        let id = id.clone();
        tauri::async_runtime::spawn(async move { write_summary_when_done(app, id, path).await });
    }

    Ok(Json(json!({ "id": id, "status": "processing", "write_to": write_to })))
}

async fn write_summary_when_done(app: AppHandle<Wry>, id: String, path: PathBuf) {
    let deadline = tokio::time::Instant::now() + SUMMARY_WAIT;
    // Give the background job time to mark itself as processing before polling
    tokio::time::sleep(Duration::from_secs(2)).await;
    while tokio::time::Instant::now() < deadline {
        let summary = match crate::summary::commands::api_get_summary(app.clone(), app.state::<AppState>(), id.clone(), None).await {
            Ok(summary) => summary,
            Err(e) => {
                warn!("Local API: cannot read summary of {}: {}", id, e);
                return;
            }
        };
        match summary.status.as_str() {
            "completed" => {
                match summary_markdown(&summary.data).map(|markdown| std::fs::write(&path, markdown)) {
                    Some(Ok(())) => info!("Local API: wrote summary of {} to {}", id, path.display()),
                    Some(Err(e)) => warn!("Local API: failed to write summary to {}: {}", path.display(), e),
                    None => warn!("Local API: summary of {} completed without content", id),
                }
                return;
            }
            "failed" | "cancelled" | "error" => {
                warn!("Local API: summary of {} ended as {}; nothing written", id, summary.status);
                return;
            }
            _ => tokio::time::sleep(Duration::from_secs(3)).await,
        }
    }
    warn!("Local API: gave up waiting for the summary of {}", id);
}

#[derive(Deserialize)]
struct ExportBody {
    /// md or txt
    format: String,
    path: String,
}

async fn export(State(state): State<ApiState>, Path(id): Path<String>, Json(body): Json<ExportBody>) -> ApiResult {
    if body.format != "md" && body.format != "txt" {
        return Err(ApiError(StatusCode::BAD_REQUEST, "format must be md or txt".into()));
    }
    let path = checked_output_path(&body.path)?;
    let meeting = load_meeting(&state, &id).await?;
    let lines = transcript_lines(&meeting.transcripts, &meeting.names);
    let document = transcript_document(&id, &meeting.title, &meeting.created_at, &lines, &body.format);
    std::fs::write(&path, document).map_err(internal)?;
    Ok(Json(json!({ "id": id, "path": path })))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(State(state): State<ApiState>, Query(query): Query<SearchQuery>) -> ApiResult {
    require_content(&state)?;
    let app_state = state.app.state::<AppState>();
    let results = TranscriptsRepository::search_transcripts(app_state.db_manager.pool(), &query.q)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "results": results })))
}

async fn list_templates() -> ApiResult {
    let templates: Vec<Value> = crate::summary::templates::list_templates()
        .into_iter()
        .map(|(id, name, description)| json!({ "id": id, "name": name, "description": description }))
        .collect();
    Ok(Json(json!({ "templates": templates })))
}

async fn recording_status() -> ApiResult {
    Ok(Json(crate::audio::recording_commands::get_recording_state().await))
}

async fn start_recording(State(state): State<ApiState>) -> ApiResult {
    if crate::audio::recording_commands::is_recording().await {
        return Err(ApiError(StatusCode::CONFLICT, "Already recording".into()));
    }
    if !crate::tray::check_can_record(&state.app).await {
        return Err(ApiError(StatusCode::CONFLICT, "Recording is not available yet (finish onboarding or download a model)".into()));
    }
    crate::tray::start_recording_via_frontend(&state.app);
    Ok(Json(json!({ "status": "starting" })))
}

async fn stop_recording(State(state): State<ApiState>) -> ApiResult {
    if !crate::audio::recording_commands::is_recording().await {
        return Err(ApiError(StatusCode::CONFLICT, "Not recording".into()));
    }
    crate::tray::stop_recording_and_save(&state.app, "Local API").await;
    Ok(Json(json!({ "status": "stopped" })))
}

// ---------- Settings commands ----------

#[tauri::command]
pub async fn get_local_api_settings(app: AppHandle<Wry>) -> Result<LocalApiSettings, String> {
    Ok(load_settings(&app))
}

#[derive(Deserialize)]
pub struct LocalApiSettingsUpdate {
    pub enabled: bool,
    pub allow_content: bool,
    #[serde(default)]
    pub regenerate_token: bool,
}

/// Saves the switches and (re)starts or stops the server. A token is created the first time it's enabled.
#[tauri::command]
pub async fn set_local_api_settings(app: AppHandle<Wry>, update: LocalApiSettingsUpdate) -> Result<LocalApiSettings, String> {
    let mut settings = load_settings(&app);
    settings.enabled = update.enabled;
    settings.allow_content = update.allow_content;
    if settings.token.is_empty() || update.regenerate_token {
        settings.token = new_token();
    }
    save_settings(&app, &settings)?;
    restart(&app, &settings);
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_bearer_token_is_accepted() {
        assert!(token_matches(Some("Bearer abc123"), "abc123"));
        assert!(!token_matches(Some("Bearer abc124"), "abc123"));
        assert!(!token_matches(Some("abc123"), "abc123"));
        assert!(!token_matches(None, "abc123"));
        assert!(!token_matches(Some("Bearer "), ""));
    }

    #[test]
    fn labels_follow_the_app() {
        let names: HashMap<String, String> = [("system-2".to_string(), "Ana".to_string())].into();
        assert_eq!(speaker_label(Some("mic"), &names).as_deref(), Some("You"));
        assert_eq!(speaker_label(Some("system"), &names).as_deref(), Some("Others"));
        assert_eq!(speaker_label(Some("system-1"), &names).as_deref(), Some("Speaker 1"));
        assert_eq!(speaker_label(Some("system-2"), &names).as_deref(), Some("Ana"));
        assert_eq!(speaker_label(None, &names), None);
    }

    #[test]
    fn documents_match_the_app_export_formats() {
        let lines = vec!["[00:01] You: hola".to_string(), "[00:05] Ana: chau".to_string()];
        assert_eq!(
            transcript_document("m-1", "Daily", "2026-10-09", &lines, "txt"),
            "Daily\n2026-10-09\n\n[00:01] You: hola\n[00:05] Ana: chau\n"
        );
        assert_eq!(
            transcript_document("m-1", "Daily", "2026-10-09", &lines, "md"),
            "# Transcript of the Meeting: m-1 - Daily\n\n## Date: 2026-10-09\n\n[00:01] You: hola  \n[00:05] Ana: chau  "
        );
    }

    #[test]
    fn output_paths_must_be_absolute_and_inside_an_existing_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(checked_output_path(dir.path().join("out.md").to_str().unwrap()).is_ok());
        assert!(checked_output_path("relative/out.md").is_err());
        assert!(checked_output_path(dir.path().join("missing/out.md").to_str().unwrap()).is_err());
    }

    #[test]
    fn summary_markdown_prefers_the_markdown_field() {
        assert_eq!(summary_markdown(&Some(json!({ "markdown": "## Hi" }))).as_deref(), Some("## Hi"));
        assert!(summary_markdown(&Some(json!({ "other": 1 }))).unwrap().contains("\"other\""));
        assert_eq!(summary_markdown(&None), None);
    }
}
