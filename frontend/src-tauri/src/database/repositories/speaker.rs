use sqlx::SqlitePool;
use std::collections::HashMap;

pub struct SpeakersRepository;

impl SpeakersRepository {
    /// Custom speaker names for a meeting, keyed by `transcripts.speaker`.
    pub async fn get_names(pool: &SqlitePool, meeting_id: &str) -> Result<HashMap<String, String>, sqlx::Error> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT speaker_key, name FROM meeting_speakers WHERE meeting_id = ?")
                .bind(meeting_id)
                .fetch_all(pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    /// Sets a speaker's display name; an empty name restores the default label.
    pub async fn set_name(
        pool: &SqlitePool,
        meeting_id: &str,
        speaker_key: &str,
        name: &str,
    ) -> Result<(), sqlx::Error> {
        let name = name.trim();
        if name.is_empty() {
            sqlx::query("DELETE FROM meeting_speakers WHERE meeting_id = ? AND speaker_key = ?")
                .bind(meeting_id)
                .bind(speaker_key)
                .execute(pool)
                .await?;
        } else {
            sqlx::query(
                "INSERT INTO meeting_speakers (meeting_id, speaker_key, name) VALUES (?, ?, ?)
                 ON CONFLICT(meeting_id, speaker_key) DO UPDATE SET name = excluded.name",
            )
            .bind(meeting_id)
            .bind(speaker_key)
            .bind(name)
            .execute(pool)
            .await?;
        }
        Ok(())
    }
}
