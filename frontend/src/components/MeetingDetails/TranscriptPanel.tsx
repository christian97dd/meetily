"use client";

import { Transcript, TranscriptSegmentData } from '@/types';
import { TranscriptView } from '@/components/TranscriptView';
import { VirtualizedTranscriptView } from '@/components/VirtualizedTranscriptView';
import { TranscriptButtonGroup } from './TranscriptButtonGroup';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { toast } from 'sonner';
import { fetchSpeakerNames, saveSpeakerName, SpeakerNames, speakerLabel } from '@/lib/speaker';
import { SpeakerRenameDialog } from './SpeakerRenameDialog';
import { SpeakerNameSuggestions, SpeakerSuggestionsDialog } from './SpeakerSuggestionsDialog';
import { invoke } from '@tauri-apps/api/core';
import type { TranscriptFormat } from '@/lib/transcript-document';

interface TranscriptPanelProps {
  transcripts: Transcript[];
  customPrompt: string;
  onPromptChange: (value: string) => void;
  onCopyTranscript: () => void;
  onExportTranscript?: (format: TranscriptFormat) => void;
  onOpenMeetingFolder: () => Promise<void>;
  isRecording: boolean;
  disableAutoScroll?: boolean;

  // Optional pagination props (when using virtualization)
  usePagination?: boolean;
  segments?: TranscriptSegmentData[];
  hasMore?: boolean;
  isLoadingMore?: boolean;
  totalCount?: number;
  loadedCount?: number;
  onLoadMore?: () => void;

  // Retranscription props
  meetingId?: string;
  meetingFolderPath?: string | null;
  onRefetchTranscripts?: () => Promise<void>;
}

export function TranscriptPanel({
  transcripts,
  customPrompt,
  onPromptChange,
  onCopyTranscript,
  onExportTranscript,
  onOpenMeetingFolder,
  isRecording,
  disableAutoScroll = false,
  usePagination = false,
  segments,
  hasMore,
  isLoadingMore,
  totalCount,
  loadedCount,
  onLoadMore,
  meetingId,
  meetingFolderPath,
  onRefetchTranscripts,
}: TranscriptPanelProps) {
  // Convert transcripts to segments if pagination is not used but we want virtualization
  const convertedSegments = useMemo(() => {
    if (usePagination && segments) {
      return segments;
    }
    // Convert transcripts to segments for virtualization
    return transcripts.map(t => ({
      id: t.id,
      timestamp: t.audio_start_time ?? 0,
      endTime: t.audio_end_time,
      text: t.text,
      confidence: t.confidence,
      speaker: t.speaker,
    }));
  }, [transcripts, usePagination, segments]);

  const [speakerNames, setSpeakerNames] = useState<SpeakerNames>({});
  const [renamingSpeaker, setRenamingSpeaker] = useState<string | null>(null);

  useEffect(() => {
    if (!meetingId) return;
    fetchSpeakerNames(meetingId).then(setSpeakerNames);
  }, [meetingId]);

  const [suggestions, setSuggestions] = useState<SpeakerNameSuggestions | null>(null);
  const [isSuggesting, setIsSuggesting] = useState(false);

  const handleSuggestSpeakerNames = useCallback(async () => {
    if (!meetingId) return;
    setIsSuggesting(true);
    try {
      setSuggestions(await invoke<SpeakerNameSuggestions>('api_suggest_speaker_names', { meetingId }));
    } catch (error) {
      console.error('Failed to suggest speaker names:', error);
      toast.error('Failed to suggest speaker names', { description: String(error) });
    } finally {
      setIsSuggesting(false);
    }
  }, [meetingId]);

  const handleApplySuggestions = useCallback(async (accepted: SpeakerNames) => {
    if (!meetingId) return;
    try {
      for (const [speaker, name] of Object.entries(accepted)) {
        await saveSpeakerName(meetingId, speaker, name);
      }
      setSpeakerNames(await fetchSpeakerNames(meetingId));
    } catch (error) {
      console.error('Failed to apply speaker names:', error);
      toast.error('Failed to apply speaker names');
    }
  }, [meetingId]);

  const handleSaveSpeakerName = useCallback(async (name: string) => {
    if (!meetingId || !renamingSpeaker) return;
    try {
      await saveSpeakerName(meetingId, renamingSpeaker, name);
      setSpeakerNames(await fetchSpeakerNames(meetingId));
    } catch (error) {
      console.error('Failed to rename speaker:', error);
      toast.error('Failed to rename speaker');
    }
  }, [meetingId, renamingSpeaker]);

  return (
    <div className="flex h-full min-w-0 w-full bg-background flex-col relative @container">
      {/* Title area */}
      <div className="p-4 border-b border-border">
        <TranscriptButtonGroup
          transcriptCount={usePagination ? (totalCount ?? convertedSegments.length) : (transcripts?.length || 0)}
          onCopyTranscript={onCopyTranscript}
          onExportTranscript={onExportTranscript}
          onSuggestSpeakerNames={meetingId && !isRecording ? handleSuggestSpeakerNames : undefined}
          isSuggestingSpeakerNames={isSuggesting}
          onOpenMeetingFolder={onOpenMeetingFolder}
          meetingId={meetingId}
          meetingFolderPath={meetingFolderPath}
          onRefetchTranscripts={onRefetchTranscripts}
        />
      </div>

      {/* Transcript content - use virtualized view for better performance */}
      <div className="flex-1 overflow-hidden pb-4">
        <VirtualizedTranscriptView
          segments={convertedSegments}
          isRecording={isRecording}
          isPaused={false}
          isProcessing={false}
          isStopping={false}
          enableStreaming={false}
          showConfidence={true}
          disableAutoScroll={disableAutoScroll}
          hasMore={hasMore}
          isLoadingMore={isLoadingMore}
          totalCount={totalCount}
          loadedCount={loadedCount}
          onLoadMore={onLoadMore}
          speakerNames={speakerNames}
          onRenameSpeaker={meetingId && !isRecording ? setRenamingSpeaker : undefined}
        />
      </div>

      <SpeakerRenameDialog
        currentLabel={renamingSpeaker ? speakerLabel(renamingSpeaker, speakerNames) : null}
        onClose={() => setRenamingSpeaker(null)}
        onSave={handleSaveSpeakerName}
      />

      <SpeakerSuggestionsDialog
        suggestions={suggestions}
        currentNames={speakerNames}
        onClose={() => setSuggestions(null)}
        onApply={handleApplySuggestions}
      />

      {/* Custom prompt input at bottom of transcript section */}
      {!isRecording && convertedSegments.length > 0 && (
        <div className="p-1 border-t border-border">
          <textarea
            placeholder="Add context for AI summary. For example people involved, meeting overview, objective etc..."
            className="w-full px-3 py-2 border border-border rounded-md text-sm focus:outline-none focus:ring-1 focus:ring-blue-500 focus:border-blue-500 bg-background shadow-sm min-h-[80px] resize-y"
            value={customPrompt}
            onChange={(e) => onPromptChange(e.target.value)}
          />
        </div>
      )}
    </div>
  );
}
