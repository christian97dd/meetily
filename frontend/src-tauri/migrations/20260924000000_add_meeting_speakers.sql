-- Display names for transcript speakers, per meeting.
-- speaker_key matches transcripts.speaker: 'mic', 'system', or 'system-N' (diarized remote speaker N)
CREATE TABLE IF NOT EXISTS meeting_speakers (
    meeting_id TEXT NOT NULL,
    speaker_key TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (meeting_id, speaker_key),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);
