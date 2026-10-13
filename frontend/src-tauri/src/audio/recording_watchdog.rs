//! Watches a live recording for the two ways it has silently died: the session stopping itself
//! after audio errors (e.g. a Bluetooth mic dropping) while the app still shows "recording", and
//! audio that simply stops arriving (e.g. CoreAudio blocking the capture). Either way it logs why,
//! tells the user, and stops the recording properly so what was captured is saved as a meeting.

use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{error, info};
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_notification::NotificationExt;

use super::recording_state::RecordingState;

const CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// No new audio for this long, while not paused, means capture is stuck. Long enough to ride out
/// the brief gaps of a device switch.
const STALL_AFTER: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq)]
pub enum Interruption {
    /// The session stopped itself; carries the last audio error, if any
    StoppedInternally(Option<String>),
    /// Audio stopped reaching the recording for this long
    Stalled(Duration),
}

impl Interruption {
    fn message(&self) -> String {
        match self {
            Interruption::StoppedInternally(Some(error)) => format!("Recording stopped: {}", error),
            Interruption::StoppedInternally(None) => "Recording stopped because of an audio error".to_string(),
            Interruption::Stalled(for_how_long) => {
                format!("No audio was captured for {}s; the recording stopped responding", for_how_long.as_secs())
            }
        }
    }
}

/// `stalled_for` is how long audio has not advanced, when that can be observed.
fn diagnose(
    session_recording: bool,
    paused: bool,
    stalled_for: Option<Duration>,
    last_error: Option<String>,
) -> Option<Interruption> {
    if !session_recording {
        return Some(Interruption::StoppedInternally(last_error));
    }
    match stalled_for {
        Some(stalled) if !paused && stalled >= STALL_AFTER => Some(Interruption::Stalled(stalled)),
        _ => None,
    }
}

/// Runs until the recording ends or is replaced. `is_live` says whether `session` is still the
/// current, user-visible recording (not stopped or being stopped by the user).
pub fn spawn<R: Runtime>(app: AppHandle<R>, session: Arc<RecordingState>, is_live: fn(&Arc<RecordingState>) -> bool) {
    tauri::async_runtime::spawn(async move {
        let mut last_progress = super::track_recorder::progress();
        let mut progressed_at = Instant::now();

        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            if !is_live(&session) {
                return;
            }

            let progress = super::track_recorder::progress();
            if progress != last_progress || session.is_paused() {
                last_progress = progress;
                progressed_at = Instant::now();
            }
            let stalled_for = progress.map(|_| progressed_at.elapsed());
            let last_error = session.get_last_error().map(|e| e.user_message().to_string());

            let Some(interruption) = diagnose(session.is_recording(), session.is_paused(), stalled_for, last_error) else {
                continue;
            };

            let message = interruption.message();
            error!("Recording watchdog: {:?}. Stopping the recording to save what was captured", interruption);
            let _ = app.emit("recording-interrupted", serde_json::json!({ "message": message }));
            if let Err(e) = app
                .notification()
                .builder()
                .title("meetily stopped recording")
                .body(format!("{}. What was captured up to then is being saved.", message))
                .show()
            {
                error!("Recording watchdog: failed to show notification: {}", e);
            }
            crate::tray::stop_recording_and_save(&app, "Recording watchdog").await;
            info!("Recording watchdog: recording stopped after interruption");
            return;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_internally_stopped_session_is_reported_with_its_error() {
        assert_eq!(
            diagnose(false, false, None, Some("Audio device was disconnected".into())),
            Some(Interruption::StoppedInternally(Some("Audio device was disconnected".into())))
        );
        assert_eq!(diagnose(false, true, None, None), Some(Interruption::StoppedInternally(None)));
    }

    #[test]
    fn audio_that_stops_arriving_is_a_stall_unless_paused() {
        let long = STALL_AFTER + Duration::from_secs(1);
        assert_eq!(diagnose(true, false, Some(long), None), Some(Interruption::Stalled(long)));
        assert_eq!(diagnose(true, true, Some(long), None), None);
        assert_eq!(diagnose(true, false, Some(STALL_AFTER - Duration::from_secs(1)), None), None);
    }

    #[test]
    fn without_tracks_only_internal_stops_are_detected() {
        assert_eq!(diagnose(true, false, None, None), None);
    }

    #[test]
    fn messages_explain_what_happened() {
        assert!(Interruption::StoppedInternally(Some("Audio device was disconnected".into())).message().contains("disconnected"));
        assert!(Interruption::Stalled(Duration::from_secs(25)).message().contains("25s"));
    }
}
