use crate::api::{TranscriptSearchResult, TranscriptSegment};
use chrono::Utc;
use sqlx::{Connection, Error as SqlxError, SqlitePool};
use tracing::{error, info};
use uuid::Uuid;

pub struct TranscriptsRepository;

impl TranscriptsRepository {
    /// Saves a new meeting and its associated transcript segments.
    /// This function uses a transaction to ensure that either both the meeting
    /// and all its transcripts are saved, or none of them are.
    pub async fn save_transcript(
        pool: &SqlitePool,
        meeting_title: &str,
        transcripts: &[TranscriptSegment],
        folder_path: Option<String>,
    ) -> Result<String, SqlxError> {
        let meeting_id = format!("meeting-{}", Uuid::new_v4());

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now();

        // 1. Create the new meeting
        let result = sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&meeting_id)
        .bind(meeting_title)
        .bind(now)
        .bind(now)
        .bind(&folder_path)
        .execute(&mut *transaction)
        .await;

        if let Err(e) = result {
            error!("Failed to create meeting '{}': {}", meeting_title, e);
            transaction.rollback().await?;
            return Err(e);
        }

        info!("Successfully created meeting with id: {}", meeting_id);

        // 2. Save each transcript segment with audio timing fields
        for segment in transcripts {
            let transcript_id = format!("transcript-{}", Uuid::new_v4());
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&transcript_id)
            .bind(&meeting_id)
            .bind(&segment.text)
            .bind(&segment.timestamp)
            .bind(segment.audio_start_time)
            .bind(segment.audio_end_time)
            .bind(segment.duration)
            .bind(&segment.speaker)
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                error!(
                    "Failed to save transcript segment for meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        info!(
            "Successfully saved {} transcript segments for meeting {}",
            transcripts.len(),
            meeting_id
        );

        // Commit the transaction
        transaction.commit().await?;

        Ok(meeting_id)
    }

    /// Searches for a query string within the transcripts.
    /// It returns a list of matching transcripts with context.
    pub async fn search_transcripts(
        pool: &SqlitePool,
        query: &str,
    ) -> Result<Vec<TranscriptSearchResult>, SqlxError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let search_query = format!("%{}%", query.to_lowercase());

        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT m.id, m.title, t.transcript, t.timestamp
             FROM meetings m
             JOIN transcripts t ON m.id = t.meeting_id
             WHERE LOWER(t.transcript) LIKE ?",
        )
        .bind(&search_query)
        .fetch_all(pool)
        .await?;

        let results = rows
            .into_iter()
            .map(|(id, title, transcript, timestamp)| {
                let match_context = Self::get_match_context(&transcript, query);
                TranscriptSearchResult {
                    id,
                    title,
                    match_context,
                    timestamp,
                }
            })
            .collect();

        Ok(results)
    }

    /// Helper function to extract a snippet of text around the first match of a query.
    fn get_match_context(transcript: &str, query: &str) -> String {
        // Work in chars, not bytes: slicing ±100 bytes split multibyte characters (á, ñ, ¿) and
        // panicked. Matching on the original chars also avoids offsets drifting when lowercasing
        // changes a character's length.
        const CONTEXT_CHARS: usize = 100;
        let chars: Vec<char> = transcript.chars().collect();
        let needle: Vec<char> = query.chars().collect();
        let same = |a: &char, b: &char| a.to_lowercase().eq(b.to_lowercase());

        let found = (!needle.is_empty() && needle.len() <= chars.len())
            .then(|| (0..=chars.len() - needle.len()).find(|&i| chars[i..i + needle.len()].iter().zip(&needle).all(|(a, b)| same(a, b))))
            .flatten();

        match found {
            Some(match_index) => {
                let start_index = match_index.saturating_sub(CONTEXT_CHARS);
                let end_index = (match_index + needle.len() + CONTEXT_CHARS).min(chars.len());

                let mut context = String::new();
                if start_index > 0 {
                    context.push_str("...");
                }
                context.extend(&chars[start_index..end_index]);
                if end_index < chars.len() {
                    context.push_str("...");
                }
                context
            }
            None => chars.iter().take(200).collect(), // Fallback to the start of the transcript
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TranscriptsRepository;

    #[test]
    fn match_context_handles_multibyte_characters_near_the_cut() {
        // 'ñ' takes bytes 99-100 and the match starts at byte 200, so a 100-byte window
        // before it would start at byte 100, in the middle of the 'ñ'
        let transcript = format!("{}ñ{}palabra{}", "a".repeat(99), "b".repeat(99), "c".repeat(150));
        assert_eq!(transcript.find("palabra"), Some(200));
        let context = TranscriptsRepository::get_match_context(&transcript, "palabra");
        assert!(context.contains("palabra"));
    }

    #[test]
    fn match_context_is_case_insensitive_with_accents() {
        let transcript = "¿Cómo andás? Hablamos del MOVIMIENTO de pedidos.";
        assert!(TranscriptsRepository::get_match_context(transcript, "movimiento").contains("MOVIMIENTO"));
        assert!(TranscriptsRepository::get_match_context(transcript, "cómo").contains("Cómo"));
    }
}
