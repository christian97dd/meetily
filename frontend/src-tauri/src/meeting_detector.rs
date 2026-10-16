//! Detects meetings: when a meeting app (browser, Zoom, Teams...) keeps the microphone open it either
//! suggests recording (notification + in-app banner) or, in auto mode, starts a recording. Either
//! way, once the app releases the microphone the recording stops after a short countdown the user
//! can cancel.
//!
//! "Meeting app using the mic" is read from CoreAudio's per-process state (macOS 14.2+), so a call
//! is detected no matter which tab or window it runs in. The detector only stops recordings it
//! started itself or that the user started from its suggestion; manual recordings are never touched.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "meeting-detection.json";
const STORE_KEY: &str = "settings";

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// While recording, CoreAudio is only asked in auto mode (to notice the meeting ending), and less often:
/// each query walks every audio process through CoreAudio's lock, which the capture also needs.
const RECORDING_QUERY_INTERVAL: Duration = Duration::from_secs(10);
/// A query stuck this long is abandoned; the detector keeps the last known state instead of waiting.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Mic must stay busy this long before recording, so a quick mic test or a dictation doesn't count.
const START_AFTER: Duration = Duration::from_secs(6);
/// Mic must stay free this long before stopping, so rejoining a call or switching devices doesn't cut it.
const STOP_AFTER: Duration = Duration::from_secs(20);
/// If the frontend hasn't started recording by then (model not ready, error), give up until the meeting ends.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Once the meeting ended, how long the user has to keep recording before it stops.
const STOP_COUNTDOWN: Duration = Duration::from_secs(10);

/// Set by the user during the stop countdown to keep the recording going.
static KEEP_RECORDING: AtomicBool = AtomicBool::new(false);

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
    /// Recording was suggested to the user, who may still start it from the suggestion.
    Offered,
    /// A recording to stop when the meeting ends: started by us or from our suggestion.
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
            Phase::Offered => {
                if is_recording {
                    self.phase = Phase::AutoRecording;
                } else if !meeting_mic_busy {
                    self.phase = Phase::Idle;
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

    /// The start was offered to the user instead of performed: don't offer it again during this meeting,
    /// and stop a recording the user starts from it once the meeting ends.
    pub fn start_handed_to_user(&mut self) {
        self.phase = Phase::Offered;
    }

    pub fn watching_meeting_end(&self) -> bool {
        self.phase == Phase::AutoRecording
    }
}

/// Whether to ask CoreAudio which apps hold the mic on this poll.
fn should_query(
    mode: DetectionMode,
    is_recording: bool,
    watching_meeting_end: bool,
    since_last_query: Option<Duration>,
) -> bool {
    match (is_recording, mode) {
        (_, DetectionMode::Off) => false,
        (false, _) => true,
        (true, _) => {
            watching_meeting_end && since_last_query.map_or(true, |elapsed| elapsed >= RECORDING_QUERY_INTERVAL)
        }
    }
}

#[derive(Debug, PartialEq)]
enum CountdownOutcome {
    Waiting,
    Stop,
    KeptByUser,
    /// The user stopped the recording during the countdown
    AlreadyStopped,
}

fn countdown_outcome(now: Instant, deadline: Instant, is_recording: bool, keep_requested: bool) -> CountdownOutcome {
    if !is_recording {
        CountdownOutcome::AlreadyStopped
    } else if keep_requested {
        CountdownOutcome::KeptByUser
    } else if now >= deadline {
        CountdownOutcome::Stop
    } else {
        CountdownOutcome::Waiting
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

/// Never configured: suggest, which records nothing unless the user accepts.
fn settings_from(stored: Option<serde_json::Value>) -> MeetingDetectionSettings {
    match stored {
        Some(value) => serde_json::from_value(value).unwrap_or_default(),
        None => MeetingDetectionSettings { mode: DetectionMode::Suggest, ..Default::default() },
    }
}

pub fn load_settings<R: Runtime>(app: &AppHandle<R>) -> MeetingDetectionSettings {
    settings_from(app.store(STORE_FILE).ok().and_then(|store| store.get(STORE_KEY)))
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
        let mut apps: Vec<String> = Vec::new();
        let mut last_query: Option<Instant> = None;
        let query_in_flight = Arc::new(AtomicBool::new(false));
        let mut stop_deadline: Option<Instant> = None;

        loop {
            interval.tick().await;

            let mode = load_settings(&app).effective_mode();
            if mode == DetectionMode::Off {
                state = AutoRecordState::default();
                apps.clear();
                if stop_deadline.take().is_some() {
                    let _ = app.emit("meeting-end-stop-cancelled", ());
                }
                continue;
            }

            let is_recording = crate::audio::recording_commands::is_recording().await;

            if let Some(deadline) = stop_deadline {
                let keep = KEEP_RECORDING.swap(false, Ordering::AcqRel);
                match countdown_outcome(Instant::now(), deadline, is_recording, keep) {
                    CountdownOutcome::Waiting => {}
                    CountdownOutcome::Stop => {
                        stop_deadline = None;
                        log::info!("Meeting detector: meeting ended, stopping recording");
                        crate::tray::stop_recording_and_save(&app, "Meeting detector").await;
                        continue;
                    }
                    outcome => {
                        stop_deadline = None;
                        log::info!("Meeting detector: stop countdown ended without stopping ({:?})", outcome);
                        let _ = app.emit("meeting-end-stop-cancelled", ());
                    }
                }
            }

            let due = should_query(mode, is_recording, state.watching_meeting_end(), last_query.map(|t| t.elapsed()));
            // Never pile up queries: if CoreAudio hasn't answered the previous one, keep the last known state
            if due && !query_in_flight.swap(true, Ordering::AcqRel) {
                last_query = Some(Instant::now());
                let in_flight = query_in_flight.clone();
                let query = tokio::task::spawn_blocking(move || {
                    let apps = meeting_apps_using_mic();
                    in_flight.store(false, Ordering::Release);
                    apps
                });
                match tokio::time::timeout(QUERY_TIMEOUT, query).await {
                    Ok(Ok(found)) => apps = found,
                    Ok(Err(_)) => query_in_flight.store(false, Ordering::Release),
                    Err(_) => log::warn!("Meeting detector: CoreAudio didn't answer in {:?}, keeping the last known state", QUERY_TIMEOUT),
                }
            }

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
                    log::info!("Meeting detector: microphone released, stopping recording in {:?} unless kept", STOP_COUNTDOWN);
                    KEEP_RECORDING.store(false, Ordering::Release);
                    stop_deadline = Some(Instant::now() + STOP_COUNTDOWN);
                    let _ = app.emit("meeting-ended-stopping", STOP_COUNTDOWN.as_secs());
                    if let Err(e) = app
                        .notification()
                        .builder()
                        .title("Meeting ended")
                        .body(format!("meetily stops recording in {} s. Open it to keep recording.", STOP_COUNTDOWN.as_secs()))
                        .show()
                    {
                        log::warn!("Meeting detector: failed to show notification: {}", e);
                    }
                }
                None => {}
            }
        }
    });
}

/// Cancels the stop pending after a meeting ended; the recording then goes on until stopped by hand.
#[tauri::command]
pub fn keep_recording_after_meeting_end() {
    KEEP_RECORDING.store(true, Ordering::Release);
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
    fn a_suggested_start_is_not_repeated() {
        let mut c = Clock::new();
        c.at(0, true, false);
        assert_eq!(c.at(6, true, false), Some(DetectorAction::StartRecording));
        c.state.start_handed_to_user();
        assert_eq!(c.at(20, true, false), None);
        assert_eq!(c.at(200, true, false), None);
    }

    #[test]
    fn a_recording_started_from_the_suggestion_stops_when_the_meeting_ends() {
        let mut c = Clock::new();
        c.at(0, true, false);
        c.at(6, true, false);
        c.state.start_handed_to_user();
        assert_eq!(c.at(30, true, true), None);
        assert!(c.state.watching_meeting_end());
        assert_eq!(c.at(100, false, true), None);
        assert_eq!(c.at(120, false, true), Some(DetectorAction::StopRecording));
    }

    #[test]
    fn an_ignored_suggestion_is_offered_again_for_the_next_meeting() {
        let mut c = Clock::new();
        c.at(0, true, false);
        c.at(6, true, false);
        c.state.start_handed_to_user();
        c.at(50, false, false);
        c.at(60, true, false);
        assert_eq!(c.at(66, true, false), Some(DetectorAction::StartRecording));
    }

    #[test]
    fn a_manual_recording_is_not_watched() {
        let mut c = Clock::new();
        c.at(0, false, true);
        assert!(!c.state.watching_meeting_end());
    }

    #[test]
    fn the_stop_waits_for_the_countdown_and_can_be_kept() {
        let start = Instant::now();
        let deadline = start + STOP_COUNTDOWN;
        assert_eq!(countdown_outcome(start, deadline, true, false), CountdownOutcome::Waiting);
        assert_eq!(countdown_outcome(deadline, deadline, true, false), CountdownOutcome::Stop);
        assert_eq!(countdown_outcome(start, deadline, true, true), CountdownOutcome::KeptByUser);
        assert_eq!(countdown_outcome(deadline, deadline, true, true), CountdownOutcome::KeptByUser);
        assert_eq!(countdown_outcome(start, deadline, false, false), CountdownOutcome::AlreadyStopped);
    }

    #[test]
    fn detection_suggests_until_the_user_chooses_otherwise() {
        assert_eq!(settings_from(None).effective_mode(), DetectionMode::Suggest);
        assert_eq!(settings_from(Some(serde_json::json!({"mode": "off"}))).effective_mode(), DetectionMode::Off);
        assert_eq!(settings_from(Some(serde_json::json!({"mode": "auto"}))).effective_mode(), DetectionMode::Auto);
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
    fn coreaudio_is_left_alone_while_recording_unless_the_meeting_end_is_watched() {
        assert!(should_query(DetectionMode::Suggest, false, false, None));
        assert!(should_query(DetectionMode::Auto, false, false, Some(Duration::from_secs(1))));
        assert!(!should_query(DetectionMode::Suggest, true, false, None));
        assert!(!should_query(DetectionMode::Auto, true, false, None));
        assert!(!should_query(DetectionMode::Off, false, false, None));
        assert!(should_query(DetectionMode::Suggest, true, true, None));
        assert!(should_query(DetectionMode::Auto, true, true, None));
        assert!(!should_query(DetectionMode::Auto, true, true, Some(Duration::from_secs(9))));
        assert!(should_query(DetectionMode::Auto, true, true, Some(Duration::from_secs(10))));
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
