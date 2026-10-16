//! Names meetings after the calendar event happening now, read from macOS Calendar (EventKit).
//! Google or Outlook calendars work once their account is added in System Settings > Internet Accounts;
//! nothing is fetched from the network by this app.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "calendar.json";
const STORE_KEY: &str = "settings";

/// Invitees without a contact name come through as their email; "ana.gomez@x.com" reads as "Ana Gomez".
fn display_name(raw: &str) -> String {
    let raw = raw.trim();
    let Some((local, _domain)) = raw.split_once('@') else {
        return raw.to_string();
    };
    local
        .split(['.', '_', '-', '+'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// People invited to a calendar event. `me` is the invitee marked as the current macOS user.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EventAttendees {
    pub me: Option<String>,
    pub others: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CalendarNamingSettings {
    pub enabled: bool,
}

/// Never configured: on. Nothing is read until macOS calendar access is granted.
fn settings_from(stored: Option<serde_json::Value>) -> CalendarNamingSettings {
    match stored {
        Some(value) => serde_json::from_value(value).unwrap_or_default(),
        None => CalendarNamingSettings { enabled: true },
    }
}

fn load_settings<R: Runtime>(app: &AppHandle<R>) -> CalendarNamingSettings {
    settings_from(app.store(STORE_FILE).ok().and_then(|store| store.get(STORE_KEY)))
}

fn save_settings<R: Runtime>(app: &AppHandle<R>, settings: &CalendarNamingSettings) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| format!("Failed to access calendar store: {}", e))?;
    store.set(STORE_KEY, serde_json::to_value(settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("Failed to save calendar settings: {}", e))
}

#[cfg(target_os = "macos")]
mod eventkit {
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_event_kit::{EKAuthorizationStatus, EKEntityType, EKEventStore, EKParticipantType};
    use objc2_foundation::{NSDate, NSError};

    use super::EventAttendees;

    /// Events that started up to this long ago are still candidates (long meetings).
    const LOOKBACK_SECS: f64 = 3.0 * 3600.0;
    /// Joining a few minutes early still picks the upcoming event.
    const LOOKAHEAD_SECS: f64 = 15.0 * 60.0;

    pub fn has_access() -> bool {
        let status = unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) };
        status == EKAuthorizationStatus::FullAccess
    }

    /// Shows the macOS permission prompt the first time; later calls return the stored answer.
    pub fn request_access() -> bool {
        let store = unsafe { EKEventStore::new() };
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(Some(tx));
        let completion = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            if let Some(tx) = tx.lock().ok().and_then(|mut guard| guard.take()) {
                let _ = tx.send(granted.as_bool());
            }
        });
        unsafe { store.requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&completion)) };
        // The store and block must stay alive until the user answers the prompt
        rx.recv_timeout(Duration::from_secs(120)).unwrap_or(false)
    }

    /// Title of the non all-day event whose start is closest to now among those not yet over.
    pub fn current_event_title() -> Option<String> {
        if !has_access() {
            return None;
        }
        unsafe {
            let store = EKEventStore::new();
            let from = NSDate::dateWithTimeIntervalSinceNow(-LOOKBACK_SECS);
            let to = NSDate::dateWithTimeIntervalSinceNow(LOOKAHEAD_SECS);
            let predicate = store.predicateForEventsWithStartDate_endDate_calendars(&from, &to, None);

            store
                .eventsMatchingPredicate(&predicate)
                .iter()
                .filter(|event| !event.isAllDay())
                .filter(|event| event.endDate().timeIntervalSinceNow() > 0.0)
                .filter(|event| event.startDate().timeIntervalSinceNow() <= LOOKAHEAD_SECS)
                .map(|event| (event.startDate().timeIntervalSinceNow().abs(), event.title().to_string()))
                .filter(|(_, title)| !title.trim().is_empty())
                .min_by(|(a, _), (b, _)| a.total_cmp(b))
                .map(|(_, title)| title.trim().to_string())
        }
    }

    /// Invitees of the non all-day event overlapping [start, end] (unix seconds) the most.
    pub fn attendees_between(start_unix: f64, end_unix: f64) -> Option<EventAttendees> {
        if !has_access() || end_unix <= start_unix {
            return None;
        }
        unsafe {
            let store = EKEventStore::new();
            let from = NSDate::dateWithTimeIntervalSince1970(start_unix);
            let to = NSDate::dateWithTimeIntervalSince1970(end_unix);
            let predicate = store.predicateForEventsWithStartDate_endDate_calendars(&from, &to, None);

            let event = store
                .eventsMatchingPredicate(&predicate)
                .iter()
                .filter(|event| !event.isAllDay())
                .map(|event| {
                    let overlap = event.endDate().timeIntervalSince1970().min(end_unix)
                        - event.startDate().timeIntervalSince1970().max(start_unix);
                    (overlap, event)
                })
                .filter(|(overlap, _)| *overlap > 0.0)
                .max_by(|(a, _), (b, _)| a.total_cmp(b))
                .map(|(_, event)| event)?;

            let mut attendees = EventAttendees::default();
            for participant in event.attendees()?.iter() {
                if participant.participantType() != EKParticipantType::Person {
                    continue;
                }
                let Some(name) = participant.name().map(|n| super::display_name(&n.to_string())) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                if participant.isCurrentUser() {
                    attendees.me = Some(name);
                } else if !attendees.others.contains(&name) {
                    attendees.others.push(name);
                }
            }
            Some(attendees)
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod eventkit {
    pub fn has_access() -> bool {
        false
    }
    pub fn request_access() -> bool {
        false
    }
    pub fn current_event_title() -> Option<String> {
        None
    }
    pub fn attendees_between(_start_unix: f64, _end_unix: f64) -> Option<super::EventAttendees> {
        None
    }
}

/// Whether the user enabled calendar naming and macOS still grants calendar access.
pub fn naming_enabled<R: Runtime>(app: &AppHandle<R>) -> bool {
    load_settings(app).enabled && eventkit::has_access()
}

/// Invitees of the calendar event that overlapped a meeting, when calendar naming is enabled.
pub async fn attendees_between<R: Runtime>(app: &AppHandle<R>, start_unix: f64, end_unix: f64) -> Option<EventAttendees> {
    if !load_settings(app).enabled {
        return None;
    }
    tokio::task::spawn_blocking(move || eventkit::attendees_between(start_unix, end_unix))
        .await
        .ok()
        .flatten()
}

#[tauri::command]
pub async fn get_calendar_naming_settings<R: Runtime>(app: AppHandle<R>) -> Result<CalendarNamingSettings, String> {
    let settings = load_settings(&app);
    // Access revoked in System Settings: report it as off instead of silently doing nothing
    Ok(CalendarNamingSettings { enabled: settings.enabled && eventkit::has_access() })
}

/// Enabling asks for calendar access; returns whether naming is enabled afterwards.
#[tauri::command]
pub async fn set_calendar_naming_enabled<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<bool, String> {
    let enabled = enabled
        && tokio::task::spawn_blocking(eventkit::request_access)
            .await
            .map_err(|e| format!("Calendar access request failed: {}", e))?;
    save_settings(&app, &CalendarNamingSettings { enabled })?;
    Ok(enabled)
}

#[tauri::command]
pub async fn get_current_calendar_event_title<R: Runtime>(app: AppHandle<R>) -> Result<Option<String>, String> {
    if !load_settings(&app).enabled {
        return Ok(None);
    }
    tokio::task::spawn_blocking(eventkit::current_event_title)
        .await
        .map_err(|e| format!("Calendar lookup failed: {}", e))
}

#[cfg(test)]
mod tests {
    use super::{display_name, settings_from};

    #[test]
    fn calendar_naming_is_on_until_the_user_turns_it_off() {
        assert!(settings_from(None).enabled);
        assert!(!settings_from(Some(serde_json::json!({"enabled": false}))).enabled);
    }

    #[test]
    fn emails_become_readable_names() {
        assert_eq!(display_name("ana.gomez@example.com"), "Ana Gomez");
        assert_eq!(display_name(" juan_perez-2@example.com "), "Juan Perez 2");
    }

    #[test]
    fn real_names_are_kept() {
        assert_eq!(display_name("  María López "), "María López");
    }
}
