//! Custom vocabulary (names, jargon) passed to Whisper as its initial prompt, which biases decoding
//! toward those spellings. Parakeet has no prompt input, so it ignores this.

use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "vocabulary.json";
const STORE_KEY: &str = "settings";

/// Whisper's prompt window is ~224 tokens and a long prompt makes it echo the prompt in silent
/// stretches, so only the first terms up to this many characters are used.
const MAX_PROMPT_CHARS: usize = 500;

static PROMPT: RwLock<Option<String>> = RwLock::new(None);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VocabularySettings {
    /// Comma- or newline-separated terms, as typed by the user.
    pub terms: String,
}

/// Normalizes the user's list into the prompt Whisper sees, or None when empty.
fn build_prompt(terms: &str) -> Option<String> {
    let mut prompt = String::new();
    for term in terms.split([',', '\n', ';']).map(str::trim).filter(|t| !t.is_empty()) {
        let next = if prompt.is_empty() { term.len() } else { prompt.len() + 2 + term.len() };
        if next > MAX_PROMPT_CHARS {
            break;
        }
        if !prompt.is_empty() {
            prompt.push_str(", ");
        }
        prompt.push_str(term);
    }
    (!prompt.is_empty()).then(|| format!("{}.", prompt))
}

/// Prompt for the current vocabulary, read by the Whisper engine on every transcription.
pub fn initial_prompt() -> Option<String> {
    PROMPT.read().ok().and_then(|prompt| prompt.clone())
}

fn apply(settings: &VocabularySettings) {
    if let Ok(mut prompt) = PROMPT.write() {
        *prompt = build_prompt(&settings.terms);
    }
}

fn load_settings<R: Runtime>(app: &AppHandle<R>) -> VocabularySettings {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(STORE_KEY))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Loads the saved vocabulary at startup.
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    apply(&load_settings(app));
}

#[tauri::command]
pub async fn get_vocabulary_settings<R: Runtime>(app: AppHandle<R>) -> Result<VocabularySettings, String> {
    Ok(load_settings(&app))
}

#[tauri::command]
pub async fn set_vocabulary_settings<R: Runtime>(app: AppHandle<R>, settings: VocabularySettings) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| format!("Failed to access vocabulary store: {}", e))?;
    store.set(STORE_KEY, serde_json::to_value(&settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("Failed to save vocabulary: {}", e))?;
    apply(&settings);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_terms_from_commas_and_lines() {
        assert_eq!(build_prompt("Acme, Kubernetes\nOKR;  SLA ").as_deref(), Some("Acme, Kubernetes, OKR, SLA."));
    }

    #[test]
    fn empty_lists_give_no_prompt() {
        assert_eq!(build_prompt(""), None);
        assert_eq!(build_prompt(" , \n ;"), None);
    }

    #[test]
    fn stops_adding_terms_past_the_limit() {
        let terms = vec!["palabra"; 200].join(", ");
        let prompt = build_prompt(&terms).unwrap();
        assert!(prompt.len() <= MAX_PROMPT_CHARS + 1);
        assert!(prompt.ends_with("palabra."));
    }
}
