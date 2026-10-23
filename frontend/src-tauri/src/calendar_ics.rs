//! Calendar events from an iCalendar feed (the "secret address in iCal format" Google Calendar
//! gives). Used where there is no system calendar to ask, like Linux.
//!
//! Covers what meeting calendars use: time zones (from the feed's own VTIMEZONE blocks), daily,
//! weekly, monthly and yearly repeats with their exceptions, moved or cancelled occurrences, and
//! invitees. All-day events are skipped, as they never are meetings.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};

#[derive(Debug, Clone, PartialEq)]
pub struct Attendee {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub attendees: Vec<Attendee>,
}

/// A parsed feed: its events and the email of the calendar's owner, when the feed names it.
#[derive(Debug, Default)]
pub struct Feed {
    events: Vec<Event>,
    timezones: HashMap<String, Vec<Onset>>,
    pub owner_email: Option<String>,
}

#[derive(Debug, Clone)]
struct Event {
    uid: String,
    title: String,
    start: Moment,
    end: Option<Moment>,
    rule: Option<Rule>,
    exceptions: Vec<Moment>,
    recurrence_id: Option<Moment>,
    cancelled: bool,
    attendees: Vec<Attendee>,
}

/// A wall-clock time and the zone it is in; `None` zone means UTC was given, `Floating` the local zone.
#[derive(Debug, Clone, PartialEq)]
struct Moment {
    local: NaiveDateTime,
    zone: Zone,
    all_day: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Zone {
    Utc,
    Named(String),
    Floating,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    frequency: Frequency,
    interval: u32,
    count: Option<u32>,
    until: Option<Moment>,
    /// (nth, weekday); nth 0 means every such weekday
    by_day: Vec<(i32, Weekday)>,
    by_month_day: Vec<i32>,
    by_month: Vec<u32>,
}

/// One STANDARD/DAYLIGHT block of a VTIMEZONE: from its onset(s) on, clocks are at `offset_to`.
#[derive(Debug, Clone)]
struct Onset {
    start: NaiveDateTime,
    offset_from: i32,
    offset_to: i32,
    rule: Option<Rule>,
}

/// Unfolds continuation lines and splits each into (name, params, value).
fn content_lines(text: &str) -> Vec<(String, HashMap<String, String>, String)> {
    let mut unfolded: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        match line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            Some(rest) if !unfolded.is_empty() => unfolded.last_mut().unwrap().push_str(rest),
            _ => unfolded.push(line.to_string()),
        }
    }

    unfolded
        .into_iter()
        .filter_map(|line| {
            // The value starts at the first colon outside a quoted parameter
            let mut in_quotes = false;
            let colon = line.char_indices().find(|&(_, c)| {
                if c == '"' {
                    in_quotes = !in_quotes;
                }
                c == ':' && !in_quotes
            })?.0;
            let (head, value) = (&line[..colon], &line[colon + 1..]);
            let mut parts = split_unquoted(head, ';').into_iter();
            let name = parts.next()?.to_ascii_uppercase();
            let params = parts
                .filter_map(|p| {
                    let (k, v) = p.split_once('=')?;
                    Some((k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
                })
                .collect();
            Some((name, params, value.to_string()))
        })
        .collect()
}

fn split_unquoted(text: &str, separator: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut in_quotes = false;
    for c in text.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
        }
        if c == separator && !in_quotes {
            parts.push(String::new());
        } else {
            parts.last_mut().unwrap().push(c);
        }
    }
    parts
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn parse_moment(value: &str, params: &HashMap<String, String>) -> Option<Moment> {
    let value = value.trim();
    if params.get("VALUE").map(String::as_str) == Some("DATE") || value.len() == 8 {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        return Some(Moment { local: date.and_hms_opt(0, 0, 0)?, zone: Zone::Floating, all_day: true });
    }
    let (text, utc) = match value.strip_suffix('Z') {
        Some(text) => (text, true),
        None => (value, false),
    };
    let local = NaiveDateTime::parse_from_str(text, "%Y%m%dT%H%M%S").ok()?;
    let zone = match (utc, params.get("TZID")) {
        (true, _) => Zone::Utc,
        (false, Some(tzid)) => Zone::Named(tzid.clone()),
        (false, None) => Zone::Floating,
    };
    Some(Moment { local, zone, all_day: false })
}

fn parse_offset(value: &str) -> Option<i32> {
    let value = value.trim();
    let sign = match value.chars().next()? {
        '-' => -1,
        '+' => 1,
        _ => return None,
    };
    let digits = &value[1..];
    let hours: i32 = digits.get(0..2)?.parse().ok()?;
    let minutes: i32 = digits.get(2..4)?.parse().ok()?;
    let seconds: i32 = digits.get(4..6).and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

fn parse_weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

fn parse_rule(value: &str) -> Option<Rule> {
    let mut rule = Rule {
        frequency: Frequency::Daily,
        interval: 1,
        count: None,
        until: None,
        by_day: Vec::new(),
        by_month_day: Vec::new(),
        by_month: Vec::new(),
    };
    let mut frequency = None;
    for part in value.split(';') {
        let Some((key, val)) = part.split_once('=') else { continue };
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => {
                frequency = Some(match val {
                    "DAILY" => Frequency::Daily,
                    "WEEKLY" => Frequency::Weekly,
                    "MONTHLY" => Frequency::Monthly,
                    "YEARLY" => Frequency::Yearly,
                    _ => return None,
                })
            }
            "INTERVAL" => rule.interval = val.parse().ok().filter(|&i| i > 0)?,
            "COUNT" => rule.count = val.parse().ok(),
            "UNTIL" => rule.until = parse_moment(val, &HashMap::new()),
            "BYDAY" => {
                for day in val.split(',') {
                    let split = day.len().checked_sub(2)?;
                    let nth = if split == 0 { 0 } else { day[..split].parse().ok()? };
                    rule.by_day.push((nth, parse_weekday(&day[split..])?));
                }
            }
            "BYMONTHDAY" => rule.by_month_day = val.split(',').filter_map(|d| d.parse().ok()).collect(),
            "BYMONTH" => rule.by_month = val.split(',').filter_map(|m| m.parse().ok()).collect(),
            _ => {}
        }
    }
    rule.frequency = frequency?;
    Some(rule)
}

/// Feed text to events. Malformed events are skipped rather than failing the whole feed.
pub fn parse(text: &str) -> Feed {
    let mut feed = Feed::default();
    let mut event: Option<Event> = None;
    let mut tz_id: Option<String> = None;
    let mut onset: Option<Onset> = None;
    let mut onset_start: Option<NaiveDateTime> = None;

    for (name, params, value) in content_lines(text) {
        match (name.as_str(), value.as_str()) {
            ("BEGIN", "VEVENT") => {
                event = Some(Event {
                    uid: String::new(),
                    title: String::new(),
                    start: Moment { local: NaiveDateTime::default(), zone: Zone::Utc, all_day: false },
                    end: None,
                    rule: None,
                    exceptions: Vec::new(),
                    recurrence_id: None,
                    cancelled: false,
                    attendees: Vec::new(),
                });
            }
            ("END", "VEVENT") => {
                if let Some(e) = event.take().filter(|e| e.start.local != NaiveDateTime::default()) {
                    feed.events.push(e);
                }
            }
            ("BEGIN", "STANDARD") | ("BEGIN", "DAYLIGHT") => {
                onset = Some(Onset { start: NaiveDateTime::default(), offset_from: 0, offset_to: 0, rule: None });
                onset_start = None;
            }
            ("END", "STANDARD") | ("END", "DAYLIGHT") => {
                if let (Some(mut o), Some(start), Some(id)) = (onset.take(), onset_start.take(), tz_id.as_ref()) {
                    o.start = start;
                    feed.timezones.entry(id.clone()).or_default().push(o);
                }
            }
            ("END", "VTIMEZONE") => tz_id = None,
            _ => {
                if let Some(o) = onset.as_mut() {
                    match name.as_str() {
                        "DTSTART" => onset_start = parse_moment(&value, &HashMap::new()).map(|m| m.local),
                        "TZOFFSETFROM" => o.offset_from = parse_offset(&value).unwrap_or(0),
                        "TZOFFSETTO" => o.offset_to = parse_offset(&value).unwrap_or(0),
                        "RRULE" => o.rule = parse_rule(&value),
                        _ => {}
                    }
                } else if let Some(e) = event.as_mut() {
                    match name.as_str() {
                        "UID" => e.uid = value,
                        "SUMMARY" => e.title = unescape(&value).trim().to_string(),
                        "DTSTART" => {
                            if let Some(m) = parse_moment(&value, &params) {
                                e.start = m;
                            }
                        }
                        "DTEND" => e.end = parse_moment(&value, &params),
                        "RRULE" => e.rule = parse_rule(&value),
                        "EXDATE" => e.exceptions.extend(value.split(',').filter_map(|v| parse_moment(v, &params))),
                        "RECURRENCE-ID" => e.recurrence_id = parse_moment(&value, &params),
                        "STATUS" => e.cancelled = value.eq_ignore_ascii_case("CANCELLED"),
                        "ATTENDEE" => {
                            let email = value
                                .get(..7)
                                .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
                                .map(|_| value[7..].trim().to_string())
                                .filter(|email| !email.is_empty());
                            let is_room = params.get("CUTYPE").is_some_and(|t| !t.eq_ignore_ascii_case("INDIVIDUAL"));
                            let name = params.get("CN").map(|cn| unescape(cn).trim().to_string()).filter(|cn| !cn.is_empty());
                            if !is_room && (name.is_some() || email.is_some()) {
                                e.attendees.push(Attendee { name, email });
                            }
                        }
                        _ => {}
                    }
                } else if name == "TZID" {
                    tz_id = Some(value);
                } else if name == "X-WR-CALNAME" && value.contains('@') {
                    feed.owner_email = Some(value.trim().to_string());
                }
            }
        }
    }
    feed
}

/// The `nth` (1-based, negative from the end) `weekday` of a month.
fn nth_weekday(year: i32, month: u32, weekday: Weekday, nth: i32) -> Option<NaiveDate> {
    if nth > 0 {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        let shift = (7 + weekday.num_days_from_monday() as i64 - first.weekday().num_days_from_monday() as i64) % 7;
        let date = first + Duration::days(shift + 7 * (nth as i64 - 1));
        (date.month() == month).then_some(date)
    } else {
        let last = NaiveDate::from_ymd_opt(year, month, days_in_month(year, month))?;
        let shift = (7 + last.weekday().num_days_from_monday() as i64 - weekday.num_days_from_monday() as i64) % 7;
        let date = last - Duration::days(shift + 7 * (-nth as i64 - 1));
        (date.month() == month).then_some(date)
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 { NaiveDate::from_ymd_opt(year + 1, 1, 1) } else { NaiveDate::from_ymd_opt(year, month + 1, 1) };
    next.map_or(31, |n| (n - Duration::days(1)).day())
}

fn months_between(from: NaiveDate, to: NaiveDate) -> i64 {
    (to.year() as i64 - from.year() as i64) * 12 + to.month() as i64 - from.month() as i64
}

/// Whether `date` is one of the days `rule` repeats on, for a series starting on `first`.
fn rule_matches(rule: &Rule, first: NaiveDate, date: NaiveDate) -> bool {
    if date < first || (!rule.by_month.is_empty() && !rule.by_month.contains(&date.month())) {
        return false;
    }
    let interval = rule.interval as i64;
    let weekday_matches = |nth_allowed: bool| {
        rule.by_day.iter().any(|&(nth, weekday)| {
            date.weekday() == weekday
                && (nth == 0 || (nth_allowed && nth_weekday(date.year(), date.month(), weekday, nth) == Some(date)))
        })
    };
    match rule.frequency {
        Frequency::Daily => (date - first).num_days() % interval == 0 && (rule.by_day.is_empty() || weekday_matches(false)),
        Frequency::Weekly => {
            let week_of = |d: NaiveDate| d - Duration::days(d.weekday().num_days_from_monday() as i64);
            let weeks = (week_of(date) - week_of(first)).num_days() / 7;
            let on_day = if rule.by_day.is_empty() { date.weekday() == first.weekday() } else { weekday_matches(false) };
            weeks % interval == 0 && on_day
        }
        Frequency::Monthly => {
            let on_day = if !rule.by_month_day.is_empty() {
                let days = days_in_month(date.year(), date.month()) as i32;
                rule.by_month_day.iter().any(|&d| date.day() as i32 == if d < 0 { days + 1 + d } else { d })
            } else if !rule.by_day.is_empty() {
                weekday_matches(true)
            } else {
                date.day() == first.day()
            };
            months_between(first, date) % interval == 0 && on_day
        }
        Frequency::Yearly => {
            let on_day = if !rule.by_day.is_empty() {
                weekday_matches(true)
            } else {
                // BYMONTH, when given, was already checked above
                (!rule.by_month.is_empty() || date.month() == first.month()) && date.day() == first.day()
            };
            (date.year() - first.year()) as i64 % interval == 0 && on_day
        }
    }
}

/// Local wall-clock times of a series that start on or before `last_day`.
fn series_starts(start: NaiveDateTime, rule: &Rule, last_day: NaiveDate, until: Option<DateTime<Utc>>, to_utc: impl Fn(NaiveDateTime) -> Option<DateTime<Utc>>) -> Vec<NaiveDateTime> {
    let mut starts = Vec::new();
    let mut produced = 0u32;
    let mut date = start.date();
    while date <= last_day {
        if rule_matches(rule, start.date(), date) {
            let local = date.and_time(start.time());
            if until.is_some_and(|until| to_utc(local).is_some_and(|utc| utc > until)) {
                break;
            }
            produced += 1;
            if rule.count.is_some_and(|count| produced > count) {
                break;
            }
            starts.push(local);
        }
        date += Duration::days(1);
    }
    starts
}

impl Feed {
    /// UTC offset (seconds) of `zone` at local time `local`.
    fn offset_at(&self, zone: &str, local: NaiveDateTime) -> Option<i32> {
        let onsets = self.timezones.get(zone)?;
        // The latest transition at or before this time decides the offset
        let mut best: Option<(NaiveDateTime, i32)> = None;
        for onset in onsets {
            let latest = match &onset.rule {
                None => (onset.start <= local).then_some(onset.start),
                // Daylight saving switches yearly: only this year's and last year's can apply
                Some(rule) if rule.frequency == Frequency::Yearly && rule.count.is_none() && rule.until.is_none() => {
                    let month = rule.by_month.first().copied().unwrap_or(onset.start.month());
                    (local.year() - 1..=local.year())
                        .filter_map(|year| match rule.by_day.first() {
                            Some(&(nth, weekday)) => nth_weekday(year, month, weekday, if nth == 0 { 1 } else { nth }),
                            None => NaiveDate::from_ymd_opt(year, month, onset.start.day()),
                        })
                        .map(|date| date.and_time(onset.start.time()))
                        .filter(|&t| t >= onset.start && t <= local)
                        .last()
                }
                Some(rule) => series_starts(onset.start, rule, local.date(), None, |_| None)
                    .into_iter()
                    .filter(|&t| t <= local)
                    .last(),
            };
            if let Some(at) = latest {
                if best.map_or(true, |(b, _)| at > b) {
                    best = Some((at, onset.offset_to));
                }
            }
        }
        best.map(|(_, offset)| offset)
            .or_else(|| onsets.iter().min_by_key(|o| o.start).map(|o| o.offset_from))
    }

    fn to_utc(&self, local: NaiveDateTime, zone: &Zone) -> Option<DateTime<Utc>> {
        match zone {
            Zone::Utc => Some(Utc.from_utc_datetime(&local)),
            Zone::Named(id) => match self.offset_at(id, local) {
                Some(offset) => Some(Utc.from_utc_datetime(&(local - Duration::seconds(offset as i64)))),
                // A zone the feed doesn't describe: assume it is the computer's own
                None => chrono::Local.from_local_datetime(&local).earliest().map(|t| t.with_timezone(&Utc)),
            },
            Zone::Floating => chrono::Local.from_local_datetime(&local).earliest().map(|t| t.with_timezone(&Utc)),
        }
    }

    /// Timed occurrences overlapping [from, to], repeats expanded.
    pub fn occurrences_between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Occurrence> {
        // Occurrences that were moved or cancelled are described by their own event
        let overridden: HashSet<(String, Option<DateTime<Utc>>)> = self
            .events
            .iter()
            .filter_map(|e| Some((e.uid.clone(), self.to_utc(e.recurrence_id.as_ref()?.local, &e.recurrence_id.as_ref()?.zone))))
            .collect();

        let mut occurrences = Vec::new();
        for event in &self.events {
            if event.start.all_day || event.cancelled {
                continue;
            }
            let Some(first_start) = self.to_utc(event.start.local, &event.start.zone) else { continue };
            let length = event
                .end
                .as_ref()
                .and_then(|end| self.to_utc(end.local, &end.zone))
                .map(|end| end - first_start)
                .filter(|length| *length > Duration::zero())
                .unwrap_or_else(|| Duration::hours(1));

            let starts: Vec<DateTime<Utc>> = match (&event.rule, &event.recurrence_id) {
                (Some(rule), None) => {
                    // Local dates can run a day ahead of UTC; one extra day covers it
                    let last_day = to.naive_utc().date() + Duration::days(1);
                    let until = rule.until.as_ref().and_then(|u| self.to_utc(u.local, &u.zone));
                    let exceptions: Vec<DateTime<Utc>> =
                        event.exceptions.iter().filter_map(|x| self.to_utc(x.local, &x.zone)).collect();
                    series_starts(event.start.local, rule, last_day, until, |local| self.to_utc(local, &event.start.zone))
                        .into_iter()
                        .filter_map(|local| self.to_utc(local, &event.start.zone))
                        .filter(|start| !exceptions.contains(start))
                        .filter(|start| !overridden.contains(&(event.uid.clone(), Some(*start))))
                        .collect()
                }
                _ => vec![first_start],
            };

            for start in starts {
                let end = start + length;
                if start <= to && end >= from {
                    occurrences.push(Occurrence {
                        title: event.title.clone(),
                        start,
                        end,
                        attendees: event.attendees.clone(),
                    });
                }
            }
        }
        occurrences
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    const FEED: &str = "BEGIN:VCALENDAR\r
X-WR-CALNAME:owner@example.com\r
BEGIN:VTIMEZONE\r
TZID:America/Argentina/Buenos_Aires\r
BEGIN:STANDARD\r
TZOFFSETFROM:-0300\r
TZOFFSETTO:-0300\r
TZNAME:-03\r
DTSTART:19700101T000000\r
END:STANDARD\r
END:VTIMEZONE\r
BEGIN:VTIMEZONE\r
TZID:Europe/Madrid\r
BEGIN:DAYLIGHT\r
TZOFFSETFROM:+0100\r
TZOFFSETTO:+0200\r
DTSTART:19700329T020000\r
RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r
END:DAYLIGHT\r
BEGIN:STANDARD\r
TZOFFSETFROM:+0200\r
TZOFFSETTO:+0100\r
DTSTART:19701025T030000\r
RRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r
END:STANDARD\r
END:VTIMEZONE\r
BEGIN:VEVENT\r
DTSTART;TZID=America/Argentina/Buenos_Aires:20260901T103000\r
DTEND;TZID=America/Argentina/Buenos_Aires:20260901T104500\r
RRULE:FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR\r
EXDATE;TZID=America/Argentina/Buenos_Aires:20261020T103000\r
UID:daily-1\r
SUMMARY:Daily\\, equipo apps\r
ATTENDEE;CN=\"Gomez, Ana\";CUTYPE=INDIVIDUAL:mailto:ana@example.com\r
ATTENDEE;CUTYPE=INDIVIDUAL:mailto:owner@example.com\r
ATTENDEE;CN=Sala 3;CUTYPE=ROOM:mailto:room@resource.example.com\r
END:VEVENT\r
BEGIN:VEVENT\r
DTSTART;TZID=America/Argentina/Buenos_Aires:20261022T120000\r
DTEND;TZID=America/Argentina/Buenos_Aires:20261022T121500\r
RECURRENCE-ID;TZID=America/Argentina/Buenos_Aires:20261022T103000\r
UID:daily-1\r
SUMMARY:Daily (moved)\r
END:VEVENT\r
BEGIN:VEVENT\r
DTSTART;TZID=America/Argentina/Buenos_Aires:20261023T103000\r
RECURRENCE-ID;TZID=America/Argentina/Buenos_Aires:20261023T103000\r
UID:daily-1\r
STATUS:CANCELLED\r
SUMMARY:Daily\r
END:VEVENT\r
BEGIN:VEVENT\r
DTSTART;TZID=Europe/Madrid:20260715T170000\r
DTEND;TZID=Europe/Madrid:20260715T180000\r
UID:madrid\r
SUMMARY:With Madrid\r
END:VEVENT\r
BEGIN:VEVENT\r
DTSTART;VALUE=DATE:20261021\r
DTEND;VALUE=DATE:20261022\r
UID:holiday\r
SUMMARY:Holiday\r
END:VEVENT\r
BEGIN:VEVENT\r
DTSTART:20261021T150000Z\r
DTEND:20261021T160000Z\r
UID:review\r
SUMMARY:Sprint review with a long\r
  folded title\r
END:VEVENT\r
END:VCALENDAR\r
";

    #[test]
    fn a_weekday_daily_repeats_in_its_own_time_zone() {
        let feed = parse(FEED);
        let found = feed.occurrences_between(utc("2026-10-19T13:00:00Z"), utc("2026-10-19T14:00:00Z"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Daily, equipo apps");
        assert_eq!(found[0].start, utc("2026-10-19T13:30:00Z"));
        assert_eq!(found[0].end, utc("2026-10-19T13:45:00Z"));
    }

    #[test]
    fn weekends_excluded_dates_and_cancelled_occurrences_have_no_daily() {
        let feed = parse(FEED);
        let titles = |from: &str, to: &str| -> Vec<String> {
            feed.occurrences_between(utc(from), utc(to)).into_iter().map(|o| o.title).collect()
        };
        assert!(titles("2026-10-18T13:00:00Z", "2026-10-18T14:00:00Z").is_empty());
        assert!(titles("2026-10-20T13:00:00Z", "2026-10-20T14:00:00Z").is_empty());
        assert!(titles("2026-10-23T13:00:00Z", "2026-10-23T14:00:00Z").is_empty());
    }

    #[test]
    fn a_moved_occurrence_replaces_the_original_slot() {
        let feed = parse(FEED);
        assert!(feed.occurrences_between(utc("2026-10-22T13:00:00Z"), utc("2026-10-22T14:00:00Z")).is_empty());
        let moved = feed.occurrences_between(utc("2026-10-22T15:00:00Z"), utc("2026-10-22T15:10:00Z"));
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].title, "Daily (moved)");
    }

    #[test]
    fn daylight_saving_comes_from_the_feed_time_zone() {
        let feed = parse(FEED);
        let found = feed.occurrences_between(utc("2026-07-15T15:00:00Z"), utc("2026-07-15T15:30:00Z"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, utc("2026-07-15T15:00:00Z"));
    }

    #[test]
    fn all_day_events_are_ignored_and_utc_and_folded_lines_are_read() {
        let feed = parse(FEED);
        let titles: Vec<String> = feed
            .occurrences_between(utc("2026-10-21T14:50:00Z"), utc("2026-10-21T15:10:00Z"))
            .into_iter()
            .map(|o| o.title)
            .collect();
        assert_eq!(titles, vec!["Sprint review with a long folded title".to_string()]);
    }

    #[test]
    fn invitees_keep_names_and_emails_but_not_rooms() {
        let feed = parse(FEED);
        assert_eq!(feed.owner_email.as_deref(), Some("owner@example.com"));
        let daily = &feed.occurrences_between(utc("2026-10-19T13:30:00Z"), utc("2026-10-19T13:31:00Z"))[0];
        assert_eq!(
            daily.attendees,
            vec![
                Attendee { name: Some("Gomez, Ana".into()), email: Some("ana@example.com".into()) },
                Attendee { name: None, email: Some("owner@example.com".into()) },
            ]
        );
    }

    #[test]
    fn repeats_stop_at_their_count_or_until() {
        let rule = parse_rule("FREQ=DAILY;COUNT=3").unwrap();
        let start = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let last = NaiveDate::from_ymd_opt(2026, 1, 31).unwrap();
        assert_eq!(series_starts(start, &rule, last, None, |_| None).len(), 3);

        let rule = parse_rule("FREQ=WEEKLY;INTERVAL=2;UNTIL=20260131T000000Z").unwrap();
        let until = Some(utc("2026-01-31T00:00:00Z"));
        let starts = series_starts(start, &rule, last, until, |local| Some(Utc.from_utc_datetime(&local)));
        let days: Vec<u32> = starts.iter().map(|s| s.day()).collect();
        assert_eq!(days, vec![1, 15, 29]);
    }

    #[test]
    fn monthly_repeats_by_weekday_or_day_of_month() {
        let start = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let last = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let second_tuesday = parse_rule("FREQ=MONTHLY;BYDAY=2TU").unwrap();
        let days: Vec<(u32, u32)> =
            series_starts(start, &second_tuesday, last, None, |_| None).iter().map(|s| (s.month(), s.day())).collect();
        assert_eq!(days, vec![(1, 13), (2, 10), (3, 10)]);

        let last_day = parse_rule("FREQ=MONTHLY;BYMONTHDAY=-1").unwrap();
        let days: Vec<u32> = series_starts(start, &last_day, last, None, |_| None).iter().map(|s| s.day()).collect();
        assert_eq!(days, vec![31, 28, 31]);
    }

    #[test]
    fn a_broken_feed_yields_no_events_instead_of_failing() {
        assert!(parse("not a calendar").occurrences_between(utc("2026-01-01T00:00:00Z"), utc("2027-01-01T00:00:00Z")).is_empty());
    }
}
