//! Names meetings after the calendar event happening now, and lists who was invited.
//!
//! On macOS it reads the system Calendar (EventKit): Google or Outlook calendars work once their
//! account is added in System Settings > Internet Accounts, and nothing is fetched by this app.
//! Elsewhere there is no system calendar, so it reads an iCal feed whose address the user pastes
//! (Google Calendar's "secret address in iCal format"); that address is secret and never logged.

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
    /// iCal feed address, used where there is no system calendar
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ics_url: Option<String>,
}

/// What the settings screen needs; the feed address itself is never sent back.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CalendarNamingStatus {
    pub enabled: bool,
    /// "system" (macOS Calendar) or "ics" (a pasted iCal feed)
    pub source: &'static str,
    pub ics_url_set: bool,
}

/// Events that started up to this long ago are still candidates (long meetings).
#[cfg(not(target_os = "macos"))]
const LOOKBACK_SECS: i64 = 3 * 3600;
/// Joining a few minutes early still picks the upcoming event.
#[cfg(any(not(target_os = "macos"), test))]
const LOOKAHEAD_SECS: i64 = 15 * 60;

/// Title of the timed event whose start is closest to `now`, among those not over yet that start
/// within the lookahead.
#[cfg(any(not(target_os = "macos"), test))]
fn current_title(occurrences: &[crate::calendar_ics::Occurrence], now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    occurrences
        .iter()
        .filter(|o| o.end > now && o.start <= now + chrono::Duration::seconds(LOOKAHEAD_SECS))
        .filter(|o| !o.title.is_empty())
        .min_by_key(|o| (o.start - now).num_seconds().abs())
        .map(|o| o.title.clone())
}

/// Invitees of the event overlapping [start, end] the most. `owner_email` identifies the user.
#[cfg(any(not(target_os = "macos"), test))]
fn attendees_of(
    occurrences: &[crate::calendar_ics::Occurrence],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    owner_email: Option<&str>,
) -> Option<EventAttendees> {
    let event = occurrences
        .iter()
        .map(|o| (o.end.min(end) - o.start.max(start), o))
        .filter(|(overlap, _)| *overlap > chrono::Duration::zero())
        .max_by_key(|(overlap, _)| *overlap)
        .map(|(_, o)| o)?;

    let mut attendees = EventAttendees::default();
    for attendee in &event.attendees {
        let Some(name) = attendee.name.as_deref().or(attendee.email.as_deref()).map(display_name) else { continue };
        if name.is_empty() {
            continue;
        }
        let is_owner = matches!((attendee.email.as_deref(), owner_email), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
        if is_owner {
            attendees.me = Some(name);
        } else if !attendees.others.contains(&name) {
            attendees.others.push(name);
        }
    }
    Some(attendees)
}

fn ics_url(settings: &CalendarNamingSettings) -> Option<&str> {
    settings.ics_url.as_deref().map(str::trim).filter(|url| !url.is_empty())
}

fn load_settings<R: Runtime>(app: &AppHandle<R>) -> CalendarNamingSettings {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(STORE_KEY))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
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

/// The iCal feed, downloaded at most every few minutes (no system calendar on this platform).
#[cfg(not(target_os = "macos"))]
mod feed {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::calendar_ics::{parse, Feed};

    const REFRESH_AFTER: Duration = Duration::from_secs(10 * 60);
    static CACHE: Mutex<Option<(String, Instant, Arc<Feed>)>> = Mutex::new(None);

    /// Errors never include the address: it grants access to the calendar.
    pub async fn load(url: &str) -> Result<Arc<Feed>, String> {
        if let Some((cached_url, fetched_at, feed)) = CACHE.lock().ok().and_then(|cache| cache.clone()) {
            if cached_url == url && fetched_at.elapsed() < REFRESH_AFTER {
                return Ok(feed);
            }
        }
        let address = match url.strip_prefix("webcal://") {
            Some(rest) => format!("https://{}", rest),
            None => url.to_string(),
        };
        if !address.starts_with("https://") {
            return Err("The calendar address must start with https:// or webcal://".to_string());
        }
        let text = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())?
            .get(&address)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|e| format!("Couldn't download the calendar ({})", e.status().map_or("network error".to_string(), |s| s.to_string())))?
            .text()
            .await
            .map_err(|_| "Couldn't read the calendar".to_string())?;
        if !text.contains("BEGIN:VCALENDAR") {
            return Err("That address isn't an iCal calendar".to_string());
        }
        let feed = Arc::new(parse(&text));
        if let Ok(mut cache) = CACHE.lock() {
            *cache = Some((url.to_string(), Instant::now(), feed.clone()));
        }
        Ok(feed)
    }
}

/// Whether calendar naming is on and its calendar can be read.
pub fn naming_enabled<R: Runtime>(app: &AppHandle<R>) -> bool {
    let settings = load_settings(app);
    #[cfg(target_os = "macos")]
    return settings.enabled && eventkit::has_access();
    #[cfg(not(target_os = "macos"))]
    return settings.enabled && ics_url(&settings).is_some();
}

/// Invitees of the calendar event that overlapped a meeting, when calendar naming is enabled.
pub async fn attendees_between<R: Runtime>(app: &AppHandle<R>, start_unix: f64, end_unix: f64) -> Option<EventAttendees> {
    let settings = load_settings(app);
    if !settings.enabled {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = ics_url(&settings);
        return tokio::task::spawn_blocking(move || eventkit::attendees_between(start_unix, end_unix))
            .await
            .ok()
            .flatten();
    }
    #[cfg(not(target_os = "macos"))]
    {
        use chrono::{TimeZone, Utc};
        let feed = match feed::load(ics_url(&settings)?).await {
            Ok(feed) => feed,
            Err(e) => {
                log::warn!("Calendar: {}", e);
                return None;
            }
        };
        let start = Utc.timestamp_opt(start_unix as i64, 0).single()?;
        let end = Utc.timestamp_opt(end_unix as i64, 0).single()?;
        attendees_of(&feed.occurrences_between(start, end), start, end, feed.owner_email.as_deref())
    }
}

#[tauri::command]
pub async fn get_calendar_naming_settings<R: Runtime>(app: AppHandle<R>) -> Result<CalendarNamingStatus, String> {
    let settings = load_settings(&app);
    let ics_url_set = ics_url(&settings).is_some();
    // Access revoked in System Settings: report it as off instead of silently doing nothing
    #[cfg(target_os = "macos")]
    return Ok(CalendarNamingStatus { enabled: settings.enabled && eventkit::has_access(), source: "system", ics_url_set });
    #[cfg(not(target_os = "macos"))]
    return Ok(CalendarNamingStatus { enabled: settings.enabled && ics_url_set, source: "ics", ics_url_set });
}

/// Enabling asks for calendar access (or needs a feed address); returns whether naming is enabled afterwards.
#[tauri::command]
pub async fn set_calendar_naming_enabled<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<bool, String> {
    let mut settings = load_settings(&app);
    #[cfg(target_os = "macos")]
    let enabled = enabled
        && tokio::task::spawn_blocking(eventkit::request_access)
            .await
            .map_err(|e| format!("Calendar access request failed: {}", e))?;
    #[cfg(not(target_os = "macos"))]
    let enabled = enabled && ics_url(&settings).is_some();
    settings.enabled = enabled;
    save_settings(&app, &settings)?;
    Ok(enabled)
}

/// Saves the iCal feed address after checking it can be read, and turns calendar naming on.
/// An empty address removes it and turns naming off.
#[tauri::command]
pub async fn set_calendar_ics_url<R: Runtime>(app: AppHandle<R>, url: String) -> Result<CalendarNamingStatus, String> {
    let url = url.trim().to_string();
    let settings = if url.is_empty() {
        CalendarNamingSettings { enabled: false, ics_url: None }
    } else {
        #[cfg(not(target_os = "macos"))]
        feed::load(&url).await?;
        CalendarNamingSettings { enabled: true, ics_url: Some(url) }
    };
    save_settings(&app, &settings)?;
    get_calendar_naming_settings(app).await
}

#[tauri::command]
pub async fn get_current_calendar_event_title<R: Runtime>(app: AppHandle<R>) -> Result<Option<String>, String> {
    let settings = load_settings(&app);
    if !settings.enabled {
        return Ok(None);
    }
    #[cfg(target_os = "macos")]
    return tokio::task::spawn_blocking(eventkit::current_event_title)
        .await
        .map_err(|e| format!("Calendar lookup failed: {}", e));
    #[cfg(not(target_os = "macos"))]
    {
        let Some(url) = ics_url(&settings) else { return Ok(None) };
        let feed = feed::load(url).await?;
        let now = chrono::Utc::now();
        let window = feed.occurrences_between(
            now - chrono::Duration::seconds(LOOKBACK_SECS),
            now + chrono::Duration::seconds(LOOKAHEAD_SECS),
        );
        Ok(current_title(&window, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar_ics::{Attendee, Occurrence};
    use chrono::{DateTime, Utc};

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    fn meeting(title: &str, start: &str, end: &str, attendees: Vec<Attendee>) -> Occurrence {
        Occurrence { title: title.into(), start: at(start), end: at(end), attendees }
    }

    #[test]
    fn the_current_title_is_the_event_starting_closest_to_now() {
        let events = vec![
            meeting("Earlier, still on", "2026-10-20T12:00:00Z", "2026-10-20T14:00:00Z", vec![]),
            meeting("Daily", "2026-10-20T13:30:00Z", "2026-10-20T13:45:00Z", vec![]),
            meeting("Over", "2026-10-20T12:00:00Z", "2026-10-20T13:00:00Z", vec![]),
        ];
        assert_eq!(current_title(&events, at("2026-10-20T13:28:00Z")).as_deref(), Some("Daily"));
        assert_eq!(current_title(&events, at("2026-10-20T12:55:00Z")).as_deref(), Some("Earlier, still on"));
        assert_eq!(current_title(&events, at("2026-10-20T15:00:00Z")), None);
    }

    #[test]
    fn feed_invitees_separate_the_owner_from_the_others() {
        let invitees = vec![
            Attendee { name: Some("Ana Gomez".into()), email: Some("ana@example.com".into()) },
            Attendee { name: None, email: Some("Owner@example.com".into()) },
            Attendee { name: None, email: Some("juan.perez@example.com".into()) },
        ];
        let events = vec![meeting("Daily", "2026-10-20T13:30:00Z", "2026-10-20T13:45:00Z", invitees)];
        let found = attendees_of(&events, at("2026-10-20T13:31:00Z"), at("2026-10-20T13:44:00Z"), Some("owner@example.com"));
        assert_eq!(
            found,
            Some(EventAttendees { me: Some("Owner".into()), others: vec!["Ana Gomez".into(), "Juan Perez".into()] })
        );
        assert_eq!(attendees_of(&events, at("2026-10-20T15:00:00Z"), at("2026-10-20T16:00:00Z"), None), None);
    }

    #[test]
    fn a_blank_feed_address_counts_as_none() {
        let settings = CalendarNamingSettings { enabled: true, ics_url: Some("  ".into()) };
        assert_eq!(ics_url(&settings), None);
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
