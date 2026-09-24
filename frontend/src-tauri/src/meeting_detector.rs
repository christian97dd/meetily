//! Detects meetings: when a meeting app (browser, Zoom, Teams...) keeps the microphone open it either
//! suggests recording (notification + in-app banner) or, in auto mode, starts a recording and stops it
//! once the app releases the microphone.
//!
//! "Meeting app using the mic" is read from CoreAudio's per-process state (macOS 14.2+), so a call
//! is detected no matter which tab or window it runs in. The detector only stops recordings it
//! started itself; manual recordings are never touched.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "meeting-detection.json";
const STORE_KEY: &str = "settings";

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Mic must stay busy this long before recording, so a quick mic test or a dictation doesn't count.
const START_AFTER: Duration = Duration::from_secs(6);
/// Mic must stay free this long before stopping, so rejoining a call or switching devices doesn't cut it.
const STOP_AFTER: Duration = Duration::from_secs(20);
/// If the frontend hasn't started recording by then (model not ready, error), give up until the meeting ends.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Bundle id prefixes of apps whose mic usage means "in a meeting". Browsers cover Google Meet;
/// Safari captures audio from the shared WebKit GPU process, hence `com.apple.WebKit`.
const MEETING_APP_BUNDLE_PREFIXES: &[&str] = &[
    "com.google.Chrome",
    "com.brave.Browser",
    "com.microsoft.edgemac",
    "company.thebrowser.Browser",
    "org.mozilla.firefox",
    "com.apple.Safari",
    "com.apple.WebKit",
    "us.zoom.xos",
    "com.microsoft.teams",
    "com.tinyspeck.slackmacgap",
    "com.hnc.Discord",
    "com.apple.FaceTime",
];

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DetectionMode {
    #[default]
    Off,
    Suggest,
    Auto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MeetingDetectionSettings {
    #[serde(default)]
    pub mode: DetectionMode,
    // Settings saved before `mode` existed only had this flag
    #[serde(default, skip_serializing)]
    auto_record: bool,
}

impl MeetingDetectionSettings {
    fn effective_mode(&self) -> DetectionMode {
        if self.mode == DetectionMode::Off && self.auto_record {
            DetectionMode::Auto
        } else {
            self.mode
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetectorAction {
    StartRecording,
    StopRecording,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Starting(Instant),
    AutoRecording,
    /// A recording the user started (or one we already handed back); never stopped by us.
    ManualRecording,
    /// Recording ended while the meeting is still on: don't restart until the mic is released.
    WaitForMeetingEnd,
}

#[derive(Debug)]
pub struct AutoRecordState {
    phase: Phase,
    busy_since: Option<Instant>,
    free_since: Option<Instant>,
}

impl Default for AutoRecordState {
    fn default() -> Self {
        Self { phase: Phase::Idle, busy_since: None, free_since: None }
    }
}

impl AutoRecordState {
    pub fn tick(&mut self, meeting_mic_busy: bool, is_recording: bool, now: Instant) -> Option<DetectorAction> {
        if meeting_mic_busy {
            self.free_since = None;
            self.busy_since.get_or_insert(now);
        } else {
            self.busy_since = None;
            self.free_since.get_or_insert(now);
        }
        let busy_for = self.busy_since.map_or(Duration::ZERO, |t| now - t);
        let free_for = self.free_since.map_or(Duration::ZERO, |t| now - t);
        let after_recording = if meeting_mic_busy { Phase::WaitForMeetingEnd } else { Phase::Idle };

        let phase = self.phase;
        match phase {
            Phase::Idle => {
                if is_recording {
                    self.phase = Phase::ManualRecording;
                } else if busy_for >= START_AFTER {
                    self.phase = Phase::Starting(now);
                    return Some(DetectorAction::StartRecording);
                }
            }
            Phase::Starting(requested_at) => {
                if is_recording {
                    self.phase = Phase::AutoRecording;
                } else if now - requested_at >= START_TIMEOUT {
                    self.phase = Phase::WaitForMeetingEnd;
                }
            }
            Phase::AutoRecording => {
                if !is_recording {
                    self.phase = after_recording;
                } else if free_for >= STOP_AFTER {
                    // Still recording until the stop completes; ManualRecording waits it out without acting.
                    self.phase = Phase::ManualRecording;
                    return Some(DetectorAction::StopRecording);
                }
            }
            Phase::ManualRecording => {
                if !is_recording {
                    self.phase = after_recording;
                }
            }
            Phase::WaitForMeetingEnd => {
                if is_recording {
                    self.phase = Phase::ManualRecording;
                } else if !meeting_mic_busy {
                    self.phase = Phase::Idle;
                }
            }
        }
        None
    }

    /// The start was offered to the user instead of performed: don't offer it again until the meeting
    /// ends, and treat a recording the user starts as manual (never auto-stopped).
    pub fn start_handed_to_user(&mut self) {
        self.phase = Phase::WaitForMeetingEnd;
    }
}

pub fn is_meeting_app(bundle_id: &str) -> bool {
    MEETING_APP_BUNDLE_PREFIXES.iter().any(|prefix| bundle_id.starts_with(prefix))
}

/// Bundle ids of meeting apps currently capturing the microphone, excluding this app.
#[cfg(target_os = "macos")]
fn meeting_apps_using_mic() -> Vec<String> {
    use cidre::core_audio as ca;

    let own_pid = std::process::id() as i32;
    let processes = match ca::System::processes() {
        Ok(processes) => processes,
        Err(e) => {
            log::warn!("Meeting detector: failed to list audio processes: {:?}", e);
            return Vec::new();
        }
    };

    processes
        .into_iter()
        .filter(|p| p.is_running_input().unwrap_or(false))
        .filter(|p| p.pid().map_or(false, |pid| pid != own_pid))
        .filter_map(|p| p.bundle_id().ok().map(|id| id.to_string()))
        .filter(|id| is_meeting_app(id))
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn meeting_apps_using_mic() -> Vec<String> {
    Vec::new()
}

pub fn load_settings<R: Runtime>(app: &AppHandle<R>) -> MeetingDetectionSettings {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(STORE_KEY))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn suggest_recording<R: Runtime>(app: &AppHandle<R>, apps: &[String]) {
    log::info!("Meeting detector: meeting detected ({:?}), suggesting to record", apps);
    let _ = app.emit("meeting-suggested", apps);
    if let Err(e) = app
        .notification()
        .builder()
        .title("Meeting detected")
        .body("Open meetily and press Record to transcribe it")
        .show()
    {
        log::warn!("Meeting detector: failed to show notification: {}", e);
    }
}

/// Polls CoreAudio and drives recording start/stop through the same paths the tray menu uses.
pub fn spawn<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let mut state = AutoRecordState::default();
        let mut suggestion_shown = false;
        let mut interval = tokio::time::interval(POLL_INTERVAL);

        loop {
            interval.tick().await;

            let mode = load_settings(&app).effective_mode();
            if mode == DetectionMode::Off {
                state = AutoRecordState::default();
                continue;
            }

            let apps = tokio::task::spawn_blocking(meeting_apps_using_mic).await.unwrap_or_default();
            let is_recording = crate::audio::recording_commands::is_recording().await;

            if suggestion_shown && (apps.is_empty() || is_recording) {
                suggestion_shown = false;
                let _ = app.emit("meeting-suggestion-cleared", ());
            }

            match state.tick(!apps.is_empty(), is_recording, Instant::now()) {
                Some(DetectorAction::StartRecording) if mode == DetectionMode::Suggest => {
                    state.start_handed_to_user();
                    suggest_recording(&app, &apps);
                    suggestion_shown = true;
                }
                Some(DetectorAction::StartRecording) => {
                    if crate::tray::check_can_record(&app).await {
                        log::info!("Meeting detector: meeting detected ({:?}), starting recording", apps);
                        crate::tray::start_recording_via_frontend(&app);
                    } else {
                        log::warn!("Meeting detector: meeting detected but recording is not available yet");
                    }
                }
                Some(DetectorAction::StopRecording) => {
                    log::info!("Meeting detector: microphone released, stopping recording");
                    crate::tray::stop_recording_and_save(&app, "Meeting detector").await;
                }
                None => {}
            }
        }
    });
}

#[tauri::command]
pub async fn get_meeting_detection_settings<R: Runtime>(app: AppHandle<R>) -> Result<MeetingDetectionSettings, String> {
    Ok(load_settings(&app))
}

#[tauri::command]
pub async fn set_meeting_detection_settings<R: Runtime>(
    app: AppHandle<R>,
    settings: MeetingDetectionSettings,
) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| format!("Failed to access meeting detection store: {}", e))?;
    let value = serde_json::to_value(&settings).map_err(|e| e.to_string())?;
    store.set(STORE_KEY, value);
    store.save().map_err(|e| format!("Failed to save meeting detection settings: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Clock {
        start: Instant,
        state: AutoRecordState,
    }

    impl Clock {
        fn new() -> Self {
            Self { start: Instant::now(), state: AutoRecordState::default() }
        }

        fn at(&mut self, secs: u64, mic_busy: bool, recording: bool) -> Option<DetectorAction> {
            self.state.tick(mic_busy, recording, self.start + Duration::from_secs(secs))
        }
    }

    #[test]
    fn starts_only_after_the_mic_stays_busy() {
        let mut c = Clock::new();
        assert_eq!(c.at(0, true, false), None);
        assert_eq!(c.at(4, true, false), None);
        assert_eq!(c.at(6, true, false), Some(DetectorAction::StartRecording));
    }

    #[test]
    fn a_short_mic_burst_does_not_start() {
        let mut c = Clock::new();
        c.at(0, true, false);
        c.at(3, false, false);
        assert_eq!(c.at(4, true, false), None);
        assert_eq!(c.at(9, true, false), None);
        assert_eq!(c.at(10, true, false), Some(DetectorAction::StartRecording));
    }

    #[test]
    fn does_not_request_start_twice_while_the_frontend_is_starting() {
        let mut c = Clock::new();
        c.at(0, true, false);
        assert_eq!(c.at(6, true, false), Some(DetectorAction::StartRecording));
        assert_eq!(c.at(8, true, false), None);
        assert_eq!(c.at(20, true, false), None);
    }

    #[test]
    fn stops_an_auto_recording_after_the_mic_stays_free() {
        let mut c = Clock::new();
        c.at(0, true, false);
        c.at(6, true, false);
        c.at(8, true, true);
        assert_eq!(c.at(100, false, true), None);
        assert_eq!(c.at(110, false, true), None);
        assert_eq!(c.at(120, false, true), Some(DetectorAction::StopRecording));
        assert_eq!(c.at(122, false, true), None);
    }

    #[test]
    fn briefly_releasing_the_mic_does_not_stop() {
        let mut c = Clock::new();
        c.at(0, true, false);
        c.at(6, true, false);
        c.at(8, true, true);
        c.at(100, false, true);
        c.at(110, true, true);
        assert_eq!(c.at(125, false, true), None);
        assert_eq!(c.at(144, false, true), None);
        assert_eq!(c.at(145, false, true), Some(DetectorAction::StopRecording));
    }

    #[test]
    fn never_stops_a_manual_recording() {
        let mut c = Clock::new();
        c.at(0, false, true);
        assert_eq!(c.at(10, true, true), None);
        assert_eq!(c.at(100, false, true), None);
        assert_eq!(c.at(200, false, true), None);
    }

    #[test]
    fn a_manual_stop_during_the_meeting_is_respected_until_it_ends() {
        let mut c = Clock::new();
        c.at(0, true, false);
        c.at(6, true, false);
        c.at(8, true, true);
        assert_eq!(c.at(30, true, false), None);
        assert_eq!(c.at(60, true, false), None);
        c.at(70, false, false);
        c.at(80, true, false);
        assert_eq!(c.at(86, true, false), Some(DetectorAction::StartRecording));
    }

    #[test]
    fn gives_up_when_the_frontend_never_starts() {
        let mut c = Clock::new();
        c.at(0, true, false);
        c.at(6, true, false);
        assert_eq!(c.at(40, true, false), None);
        assert_eq!(c.at(100, true, false), None);
    }

    #[test]
    fn a_suggested_start_is_not_repeated_nor_auto_stopped() {
        let mut c = Clock::new();
        c.at(0, true, false);
        assert_eq!(c.at(6, true, false), Some(DetectorAction::StartRecording));
        c.state.start_handed_to_user();
        assert_eq!(c.at(20, true, false), None);
        // The user presses Record: that recording is theirs
        assert_eq!(c.at(30, true, true), None);
        assert_eq!(c.at(100, false, true), None);
        assert_eq!(c.at(200, false, true), None);
    }

    #[test]
    fn legacy_auto_record_flag_maps_to_auto_mode() {
        let legacy: MeetingDetectionSettings = serde_json::from_str(r#"{"auto_record":true}"#).unwrap();
        assert_eq!(legacy.effective_mode(), DetectionMode::Auto);
        let current: MeetingDetectionSettings = serde_json::from_str(r#"{"mode":"suggest"}"#).unwrap();
        assert_eq!(current.effective_mode(), DetectionMode::Suggest);
        assert_eq!(MeetingDetectionSettings::default().effective_mode(), DetectionMode::Off);
    }

    #[test]
    fn recognises_meeting_apps_by_bundle_prefix() {
        assert!(is_meeting_app("com.google.Chrome.helper"));
        assert!(is_meeting_app("us.zoom.xos"));
        assert!(is_meeting_app("com.apple.WebKit.GPU"));
        assert!(!is_meeting_app("com.apple.VoiceMemos"));
        assert!(!is_meeting_app("com.openai.chat"));
    }
}
