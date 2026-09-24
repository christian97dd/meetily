"use client";

import { useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { Mic, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useRecordingState } from '@/contexts/RecordingStateContext';

/**
 * Shown when the meeting detector (in "suggest" mode) sees a meeting app holding the microphone.
 * Nothing is recorded until the user presses Record.
 */
export function MeetingSuggestionBanner() {
  const [visible, setVisible] = useState(false);
  const { isRecording } = useRecordingState();

  useEffect(() => {
    const unlisteners = [
      listen('meeting-suggested', () => setVisible(true)),
      listen('meeting-suggestion-cleared', () => setVisible(false)),
    ];
    return () => {
      unlisteners.forEach(p => p.then(unlisten => unlisten()));
    };
  }, []);

  useEffect(() => {
    if (isRecording) setVisible(false);
  }, [isRecording]);

  if (!visible) return null;

  const handleRecord = () => {
    setVisible(false);
    // Same start path as the tray menu: the home page picks this flag up and starts recording
    sessionStorage.setItem('autoStartRecording', 'true');
    window.location.assign('/');
  };

  return (
    <div className="fixed top-4 left-1/2 -translate-x-1/2 z-50 flex items-center gap-3 rounded-lg border border-border bg-background px-4 py-3 shadow-lg">
      <Mic className="h-4 w-4 text-blue-600 dark:text-blue-400" />
      <span className="text-sm text-foreground">Meeting detected</span>
      <Button size="sm" onClick={handleRecord}>Record</Button>
      <button
        type="button"
        className="text-muted-foreground/70 hover:text-foreground"
        onClick={() => setVisible(false)}
        title="Dismiss"
      >
        <X className="h-4 w-4" />
      </button>
    </div>
  );
}
