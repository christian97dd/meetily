"use client";

import { useEffect } from 'react';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';

/**
 * Shows why a recording stopped on its own (audio device lost, capture stuck). The backend
 * watchdog has already stopped it and is saving what was captured.
 */
export function RecordingInterruptionListener() {
  useEffect(() => {
    const unlisten = listen<{ message: string }>('recording-interrupted', ({ payload }) => {
      toast.error('Recording stopped', {
        description: `${payload.message}. What was captured up to then is being saved.`,
        duration: Infinity,
      });
    });
    return () => {
      unlisten.then(fn => fn());
    };
  }, []);

  return null;
}
