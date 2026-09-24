import { invoke } from '@tauri-apps/api/core';
import type { TranscriptSpeaker } from '@/types';

/** Custom display names for a meeting, keyed by transcript speaker. */
export type SpeakerNames = Record<string, string>;

const DIARIZED_SPEAKER = /^system-(\d+)$/;

function defaultSpeakerLabel(speaker: string): string | null {
  if (speaker === 'mic') return 'You';
  if (speaker === 'system') return 'Others';
  const diarized = DIARIZED_SPEAKER.exec(speaker);
  return diarized ? `Speaker ${diarized[1]}` : null;
}

export function isKnownSpeaker(speaker: string | undefined): speaker is TranscriptSpeaker {
  return !!speaker && defaultSpeakerLabel(speaker) !== null;
}

export function speakerLabel(speaker: string | undefined, names?: SpeakerNames): string | null {
  if (!isKnownSpeaker(speaker)) return null;
  return names?.[speaker] || defaultSpeakerLabel(speaker);
}

export function withSpeakerPrefix(text: string, speaker: string | undefined, names?: SpeakerNames): string {
  const label = speakerLabel(speaker, names);
  return label ? `${label}: ${text}` : text;
}

export async function fetchSpeakerNames(meetingId: string): Promise<SpeakerNames> {
  try {
    return await invoke<SpeakerNames>('api_get_meeting_speakers', { meetingId });
  } catch (error) {
    console.error('Failed to load speaker names:', error);
    return {};
  }
}

export function saveSpeakerName(meetingId: string, speakerKey: string, name: string): Promise<void> {
  return invoke('api_set_meeting_speaker', { meetingId, speakerKey, name });
}
