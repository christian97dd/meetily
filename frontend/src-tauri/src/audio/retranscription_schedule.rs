//! Re-transcribes pending meetings once a day at a time the user picks, instead of right after
//! each recording (back-to-back meetings made the batch job share the Whisper engine with a live
//! recording). "Pending" means it has separate mic/system tracks and was never re-transcribed.
//!
//! A recording always wins: the run waits while one is in progress, and starting a recording
//! cancels it; it resumes on the next minute check until every pending meeting is done that day.

use std::path::Path;
use std::time::Duration;

use chrono::{Local, NaiveDate, NaiveTime};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_store::StoreExt;

use crate::state::AppState;

const STORE_FILE: &str = "retranscription-schedule.json";
const STORE_KEY: &str = "settings";
const LAST_RUN_KEY: &str = "last_completed_date";
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// No latency budget when it runs unattended, so the slower full model is worth it
const DEFAULT_MODEL: &str = "large-v3";
const DEFAULT_TIME: &str = "22:00";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetranscriptionSchedule {
    pub enabled: bool,
    /// Local time of day, "HH:MM"
    pub time: String,
    /// Whisper model; falls back to the configured live model if this one isn't downloaded
    pub model: String,
}

impl Default for RetranscriptionSchedule {
    fn default() -> Self {
        Self { enabled: false, time: DEFAULT_TIME.to_string(), model: DEFAULT_MODEL.to_string() }
    }
}

fn parse_time(time: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(time.trim(), "%H:%M").ok()
}

/// Due once the scheduled time has passed today and today's run hasn't completed yet.
fn is_due(now: chrono::NaiveDateTime, scheduled: NaiveTime, last_completed: Option<NaiveDate>) -> bool {
    now.time() >= scheduled && last_completed != Some(now.date())
}

fn load_settings<R: Runtime>(app: &AppHandle<R>) -> RetranscriptionSchedule {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(STORE_KEY))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn last_completed<R: Runtime>(app: &AppHandle<R>) -> Option<NaiveDate> {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(LAST_RUN_KEY))
        .and_then(|value| value.as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()))
}

fn mark_completed<R: Runtime>(app: &AppHandle<R>, date: NaiveDate) {
    if let Ok(store) = app.store(STORE_FILE) {
        store.set(LAST_RUN_KEY, serde_json::json!(date.format("%Y-%m-%d").to_string()));
        if let Err(e) = store.save() {
            warn!("Scheduled re-transcription: failed to save last run date: {}", e);
        }
    }
}

/// Meetings with separate tracks never re-transcribed, oldest first: (id, folder).
async fn pending_meetings(pool: &SqlitePool) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.id, m.folder_path FROM meetings m
         WHERE m.folder_path IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM meeting_retranscriptions r WHERE r.meeting_id = m.id)
         ORDER BY m.created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, folder)| super::track_recorder::existing_tracks(Path::new(folder)).is_some())
        .collect())
}

/// Re-transcribes pending meetings one by one. Returns true when none is left, false if it had to
/// stop (a recording started, or a meeting failed and should be retried later).
async fn run_pending<R: Runtime>(app: &AppHandle<R>, settings: &RetranscriptionSchedule) -> bool {
    let pending = {
        let state = app.state::<AppState>();
        match pending_meetings(state.db_manager.pool()).await {
            Ok(pending) => pending,
            Err(e) => {
                warn!("Scheduled re-transcription: cannot list pending meetings: {}", e);
                return false;
            }
        }
    };
    if !pending.is_empty() {
        info!("Scheduled re-transcription: {} pending meeting(s)", pending.len());
    }

    let language = crate::get_language_preference_internal();
    let provider = Some("whisper".to_string());
    for (meeting_id, folder) in pending {
        if super::recording_commands::is_recording().await {
            info!("Scheduled re-transcription: paused while recording");
            return false;
        }
        let mut result = super::retranscription::start_retranscription(
            app.clone(), meeting_id.clone(), folder.clone(), language.clone(), Some(settings.model.clone()), provider.clone(),
        )
        .await;
        if let Err(e) = &result {
            if super::recording_commands::is_recording().await {
                return false;
            }
            // Usually the chosen model isn't downloaded; the live model still beats the live transcript
            warn!("Scheduled re-transcription with {} failed for {} ({}), retrying with the configured model", settings.model, meeting_id, e);
            result = super::retranscription::start_retranscription(
                app.clone(), meeting_id.clone(), folder, language.clone(), None, provider.clone(),
            )
            .await;
        }
        if let Err(e) = result {
            warn!("Scheduled re-transcription failed for {}: {}", meeting_id, e);
            return false;
        }
    }
    true
}

/// Checks every minute whether today's run is due.
pub fn spawn<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        loop {
            interval.tick().await;
            let settings = load_settings(&app);
            let Some(scheduled) = settings.enabled.then(|| parse_time(&settings.time)).flatten() else {
                continue;
            };
            let now = Local::now().naive_local();
            if !is_due(now, scheduled, last_completed(&app))
                || super::recording_commands::is_recording().await
                || super::retranscription::is_retranscription_in_progress()
            {
                continue;
            }
            if run_pending(&app, &settings).await {
                mark_completed(&app, now.date());
            }
        }
    });
}

#[tauri::command]
pub async fn get_retranscription_schedule<R: Runtime>(app: AppHandle<R>) -> Result<RetranscriptionSchedule, String> {
    Ok(load_settings(&app))
}

#[tauri::command]
pub async fn set_retranscription_schedule<R: Runtime>(app: AppHandle<R>, settings: RetranscriptionSchedule) -> Result<(), String> {
    if parse_time(&settings.time).is_none() {
        return Err(format!("Invalid time \"{}\", expected HH:MM", settings.time));
    }
    let store = app.store(STORE_FILE).map_err(|e| format!("Failed to access schedule store: {}", e))?;
    store.set(STORE_KEY, serde_json::to_value(&settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("Failed to save schedule: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(date: &str, time: &str) -> chrono::NaiveDateTime {
        NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap().and_time(parse_time(time).unwrap())
    }

    #[test]
    fn runs_once_after_the_scheduled_time_each_day() {
        let scheduled = parse_time("22:00").unwrap();
        assert!(!is_due(at("2026-10-13", "21:59"), scheduled, None));
        assert!(is_due(at("2026-10-13", "22:00"), scheduled, None));
        assert!(is_due(at("2026-10-13", "23:30"), scheduled, None), "a late start still runs today");
        let done = NaiveDate::parse_from_str("2026-10-13", "%Y-%m-%d").ok();
        assert!(!is_due(at("2026-10-13", "23:30"), scheduled, done));
        assert!(!is_due(at("2026-10-14", "08:00"), scheduled, done), "waits for the next day's time");
        assert!(is_due(at("2026-10-14", "22:01"), scheduled, done));
    }

    #[test]
    fn parses_only_hours_and_minutes() {
        assert!(parse_time("07:30").is_some());
        assert!(parse_time(" 22:00 ").is_some());
        assert!(parse_time("25:00").is_none());
        assert!(parse_time("10pm").is_none());
        assert!(parse_time("").is_none());
    }
}
