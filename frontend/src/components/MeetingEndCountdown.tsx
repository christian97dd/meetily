"use client";

import { useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';

const TOAST_ID = 'meeting-end-countdown';

/**
 * The meeting detector saw the meeting end and will stop the recording after a countdown;
 * lets the user keep recording instead.
 */
export function MeetingEndCountdown() {
  useEffect(() => {
    const unlisteners = [
      listen<number>('meeting-ended-stopping', ({ payload: seconds }) => {
        toast('Meeting ended', {
          id: TOAST_ID,
          description: `Recording stops in ${seconds} s.`,
          duration: (seconds + 2) * 1000,
          action: {
            label: 'Keep recording',
            onClick: () => {
              invoke('keep_recording_after_meeting_end').catch(error =>
                console.error('Failed to keep recording:', error)
              );
            },
          },
        });
      }),
      listen('meeting-end-stop-cancelled', () => toast.dismiss(TOAST_ID)),
    ];
    return () => {
      unlisteners.forEach(p => p.then(unlisten => unlisten()));
    };
  }, []);

  return null;
}
