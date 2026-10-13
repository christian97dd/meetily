-- Meetings whose recording was re-transcribed in batch, so scheduled runs only pick pending ones
CREATE TABLE IF NOT EXISTS meeting_retranscriptions (
    meeting_id TEXT PRIMARY KEY NOT NULL,
    completed_at TEXT NOT NULL,
    model TEXT,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);
