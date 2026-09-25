//! Suggests who each transcript speaker is, using the invitees of the meeting's calendar event and
//! how people address each other in the transcript ("gracias, Juan"). Nothing biometric is stored:
//! voices are only grouped per meeting; names come from the calendar and the conversation.
//!
//! The configured summary model does the matching. Its answer is validated so it can only pick
//! speakers that exist in the meeting and names that are on the invite list.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use tauri::{AppHandle, Manager, Runtime};

use crate::database::repositories::{meeting::MeetingsRepository, setting::SettingsRepository};
use crate::database::models::Transcript;
use crate::state::AppState;
use crate::summary::llm_client::{generate_summary, LLMProvider};

/// Lines kept around each mention: the addressed person usually speaks right before or after.
const CONTEXT_LINES: usize = 2;
/// Keeps the prompt within small local models' context windows.
const MAX_PROMPT_LINES: usize = 80;
/// The calendar event may have started a bit before the recording or ended after it.
const CALENDAR_SLACK_SECS: f64 = 10.0 * 60.0;

const SYSTEM_PROMPT: &str = "You match meeting speakers to people. Speakers are labeled like system-1, system-2. \
Use only how people address or refer to each other in the transcript. \
Answer with a single JSON object mapping speaker label to a name from the provided list, for example {\"system-1\": \"Ana Pérez\"}. \
Omit speakers you are not confident about. Never invent names that are not in the list. No explanations.";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SpeakerNameSuggestions {
    /// Suggested display name per speaker key ('mic', 'system-N').
    pub names: HashMap<String, String>,
    /// Invitees found in the calendar; empty when calendar naming is off or no event matched.
    pub attendees: Vec<String>,
}

struct Line<'a> {
    speaker: &'a str,
    text: &'a str,
    start: Option<f64>,
}

fn first_name(full_name: &str) -> &str {
    full_name.split_whitespace().next().unwrap_or(full_name)
}

fn mentions(text: &str, word: &str) -> bool {
    let word = word.to_lowercase();
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|token| token == word)
}

/// Indexes of lines mentioning an attendee (full or first name) plus their neighbors, in order.
fn context_line_indexes(lines: &[Line], attendees: &[String]) -> Vec<usize> {
    let mut keep = BTreeSet::new();
    for (i, line) in lines.iter().enumerate() {
        let mentioned = attendees.iter().any(|name| {
            mentions(line.text, first_name(name)) || name.split_whitespace().skip(1).any(|part| mentions(line.text, part))
        });
        if mentioned {
            keep.extend(i.saturating_sub(CONTEXT_LINES)..=(i + CONTEXT_LINES).min(lines.len() - 1));
        }
    }
    keep.into_iter().take(MAX_PROMPT_LINES).collect()
}

/// Keeps only answers naming an existing diarized speaker and an invitee (matched case-insensitively).
fn parse_suggestions(answer: &str, speakers: &BTreeSet<&str>, attendees: &[String]) -> HashMap<String, String> {
    let (Some(start), Some(end)) = (answer.find('{'), answer.rfind('}')) else {
        return HashMap::new();
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&answer[start..=end]) else {
        return HashMap::new();
    };

    let mut used = BTreeSet::new();
    map.into_iter()
        .filter(|(speaker, _)| speaker.starts_with("system-") && speakers.contains(speaker.as_str()))
        .filter_map(|(speaker, name)| {
            let name = name.as_str()?.trim().to_lowercase();
            let attendee = attendees.iter().find(|a| a.to_lowercase() == name || first_name(a).to_lowercase() == name)?;
            // Two speakers can't be the same person
            used.insert(attendee.clone()).then(|| (speaker, attendee.clone()))
        })
        .collect()
}

fn format_time(seconds: Option<f64>) -> String {
    let secs = seconds.unwrap_or(0.0).max(0.0) as u64;
    format!("[{:02}:{:02}]", secs / 60, secs % 60)
}

fn build_prompt(lines: &[Line], indexes: &[usize], attendees: &[String]) -> String {
    let mut prompt = format!("People in this meeting: {}\n\nTranscript excerpts:\n", attendees.join(", "));
    let mut previous = None;
    for &i in indexes {
        if previous.is_some_and(|p: usize| i > p + 1) {
            prompt.push_str("...\n");
        }
        let line = &lines[i];
        prompt.push_str(&format!("{} {}: {}\n", format_time(line.start), line.speaker, line.text));
        previous = Some(i);
    }
    prompt
}

struct ResolvedLlm {
    provider: LLMProvider,
    model: String,
    api_key: String,
    ollama_endpoint: Option<String>,
    custom_openai_endpoint: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
}

/// Same provider settings the summary uses.
async fn resolve_llm(pool: &sqlx::SqlitePool) -> Result<ResolvedLlm, String> {
    let config = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| format!("Failed to read model settings: {}", e))?
        .ok_or("No summary model configured")?;
    let provider = LLMProvider::from_str(&config.provider)?;

    let mut resolved = ResolvedLlm {
        provider: provider.clone(),
        model: config.model.clone(),
        api_key: String::new(),
        ollama_endpoint: None,
        custom_openai_endpoint: None,
        max_tokens: None,
        temperature: None,
        top_p: None,
    };
    match provider {
        LLMProvider::BuiltInAI => {}
        LLMProvider::Ollama => resolved.ollama_endpoint = config.ollama_endpoint,
        LLMProvider::CustomOpenAI => {
            let custom = SettingsRepository::get_custom_openai_config(pool)
                .await
                .map_err(|e| format!("Failed to read custom OpenAI settings: {}", e))?
                .ok_or("Custom OpenAI provider selected but not configured")?;
            resolved.custom_openai_endpoint = Some(custom.endpoint);
            resolved.api_key = custom.api_key.unwrap_or_default();
            resolved.max_tokens = custom.max_tokens.map(|t| t as u32);
            resolved.temperature = custom.temperature;
            resolved.top_p = custom.top_p;
        }
        _ => {
            resolved.api_key = SettingsRepository::get_api_key(pool, &config.provider)
                .await
                .map_err(|e| format!("Failed to read API key: {}", e))?
                .filter(|key| !key.is_empty())
                .ok_or_else(|| format!("API key not found for {}", config.provider))?;
        }
    }
    Ok(resolved)
}

#[tauri::command]
pub async fn api_suggest_speaker_names<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<SpeakerNameSuggestions, String> {
    let pool = state.db_manager.pool();

    let meeting = MeetingsRepository::get_meeting_metadata(pool, &meeting_id)
        .await
        .map_err(|e| format!("Failed to load meeting: {}", e))?
        .ok_or("Meeting not found")?;
    let (transcripts, _): (Vec<Transcript>, i64) =
        MeetingsRepository::get_meeting_transcripts_paginated(pool, &meeting_id, i64::MAX, 0)
            .await
            .map_err(|e| format!("Failed to load transcripts: {}", e))?;

    // Meetings are saved when recording stops, so created_at marks the end
    let end = meeting.created_at.0.timestamp() as f64;
    let duration = transcripts.iter().filter_map(|t| t.audio_end_time).fold(0.0, f64::max);
    let attendees = crate::calendar::attendees_between(
        &app,
        end - duration - CALENDAR_SLACK_SECS,
        end + CALENDAR_SLACK_SECS,
    )
    .await
    .unwrap_or_default();

    let mut names = HashMap::new();
    if let Some(me) = &attendees.me {
        names.insert("mic".to_string(), me.clone());
    }

    let lines: Vec<Line> = transcripts
        .iter()
        .filter_map(|t| Some(Line { speaker: t.speaker.as_deref()?, text: &t.transcript, start: t.audio_start_time }))
        .collect();
    let speakers: BTreeSet<&str> = lines.iter().map(|l| l.speaker).filter(|s| s.starts_with("system-")).collect();
    let indexes = if attendees.others.is_empty() || speakers.is_empty() {
        Vec::new()
    } else {
        context_line_indexes(&lines, &attendees.others)
    };

    if !indexes.is_empty() {
        let llm = resolve_llm(pool).await?;
        let prompt = build_prompt(&lines, &indexes, &attendees.others);
        let app_data_dir = app.path().app_data_dir().ok();
        let completion = generate_summary(
            &reqwest::Client::new(),
            &llm.provider,
            &llm.model,
            &llm.api_key,
            SYSTEM_PROMPT,
            &prompt,
            llm.ollama_endpoint.as_deref(),
            llm.custom_openai_endpoint.as_deref(),
            llm.max_tokens,
            llm.temperature,
            llm.top_p,
            app_data_dir.as_ref(),
            None,
        )
        .await?;
        names.extend(parse_suggestions(&completion.content, &speakers, &attendees.others));
    }

    let mut all_attendees = attendees.others;
    if let Some(me) = attendees.me {
        all_attendees.insert(0, me);
    }
    Ok(SpeakerNameSuggestions { names, attendees: all_attendees })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(speaker: &'a str, text: &'a str) -> Line<'a> {
        Line { speaker, text, start: None }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keeps_mentions_with_their_neighbors_only() {
        let lines = vec![
            line("system-1", "arrancamos"),
            line("system-2", "dale"),
            line("mic", "bueno"),
            line("system-1", "gracias, Juan, muy claro"),
            line("system-2", "de nada"),
            line("mic", "sigamos"),
            line("system-1", "ok"),
            line("system-2", "listo"),
            line("mic", "nada más"),
        ];
        assert_eq!(context_line_indexes(&lines, &names(&["Juan Pérez"])), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn matches_whole_words_and_last_names() {
        let lines = vec![line("system-1", "Juana dijo algo"), line("system-2", "lo vio Pérez")];
        assert_eq!(context_line_indexes(&lines, &names(&["Juan Pérez"])), vec![0, 1]);
        assert!(!mentions("Juana dijo algo", "Juan"));
    }

    #[test]
    fn no_mentions_means_nothing_to_ask() {
        let lines = vec![line("system-1", "hola"), line("system-2", "chau")];
        assert!(context_line_indexes(&lines, &names(&["Ana"])).is_empty());
    }

    #[test]
    fn parses_only_existing_speakers_and_invitees() {
        let speakers: BTreeSet<&str> = ["system-1", "system-2"].into_iter().collect();
        let attendees = names(&["Juan Pérez", "Ana Gómez"]);
        let answer = r#"Sure! {"system-1": "juan pérez", "system-2": "Ana", "system-9": "Ana Gómez", "mic": "Juan Pérez", "system-3": "Pedro"}"#;
        let parsed = parse_suggestions(answer, &speakers, &attendees);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["system-1"], "Juan Pérez");
        assert_eq!(parsed["system-2"], "Ana Gómez");
    }

    #[test]
    fn one_person_is_never_assigned_to_two_speakers() {
        let speakers: BTreeSet<&str> = ["system-1", "system-2"].into_iter().collect();
        let parsed = parse_suggestions(r#"{"system-1": "Ana", "system-2": "Ana"}"#, &speakers, &names(&["Ana"]));
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn garbage_answers_yield_no_suggestions() {
        let speakers: BTreeSet<&str> = ["system-1"].into_iter().collect();
        assert!(parse_suggestions("I don't know", &speakers, &names(&["Ana"])).is_empty());
        assert!(parse_suggestions("{not json}", &speakers, &names(&["Ana"])).is_empty());
    }

    #[test]
    fn prompt_marks_gaps_between_excerpts() {
        let lines = vec![line("system-1", "a"), line("system-2", "b"), line("system-1", "c"), line("system-2", "d")];
        let prompt = build_prompt(&lines, &[0, 1, 3], &names(&["Ana"]));
        assert!(prompt.contains("system-2: b\n...\n[00:00] system-2: d"));
    }
}
