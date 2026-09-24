import type { TranscriptSpeaker } from '@/types';

const SPEAKER_LABELS: Record<TranscriptSpeaker, string> = {
  mic: 'You',
  system: 'Others',
};

export function speakerLabel(speaker: string | undefined): string | null {
  if (speaker === 'mic' || speaker === 'system') {
    return SPEAKER_LABELS[speaker];
  }
  return null;
}

export function withSpeakerPrefix(text: string, speaker: string | undefined): string {
  const label = speakerLabel(speaker);
  return label ? `${label}: ${text}` : text;
}
