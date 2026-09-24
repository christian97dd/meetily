import type { Transcript } from '@/types';
import { SpeakerNames, withSpeakerPrefix } from '@/lib/speaker';

export type TranscriptFormat = 'md' | 'txt';

interface TranscriptDocumentMeeting {
  id: string;
  title: string;
  created_at: string;
}

// Recording-relative [MM:SS]; old transcripts without audio timing fall back to wall-clock time
function formatTime(seconds: number | undefined, fallbackTimestamp: string): string {
  if (seconds === undefined) {
    return fallbackTimestamp;
  }
  const totalSecs = Math.floor(seconds);
  const mins = Math.floor(totalSecs / 60);
  const secs = totalSecs % 60;
  return `[${mins.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}]`;
}

export function buildTranscriptDocument(
  meeting: TranscriptDocumentMeeting,
  title: string,
  transcripts: Transcript[],
  speakerNames: SpeakerNames,
  format: TranscriptFormat,
): string {
  const date = new Date(meeting.created_at).toLocaleDateString();
  const lines = transcripts.map(
    t => `${formatTime(t.audio_start_time, t.timestamp)} ${withSpeakerPrefix(t.text, t.speaker, speakerNames)}`,
  );

  if (format === 'txt') {
    return `${title}\n${date}\n\n${lines.join('\n')}\n`;
  }
  // Two trailing spaces force a markdown line break between segments
  return `# Transcript of the Meeting: ${meeting.id} - ${title}\n\n## Date: ${date}\n\n${lines.map(l => `${l}  `).join('\n')}`;
}

export function transcriptFileName(title: string, format: TranscriptFormat): string {
  const safeTitle = title.replace(/[\\/:*?"<>|]+/g, '-').trim() || 'transcript';
  return `${safeTitle}.${format}`;
}
