"use client";

import { useEffect, useState } from 'react';
import { useRouter } from 'next/navigation';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { SpeakerNames, speakerLabel } from '@/lib/speaker';

export interface SpeakerNameSuggestions {
  names: SpeakerNames;
  attendees: string[];
  calendar_enabled: boolean;
}

interface SpeakerSuggestionsDialogProps {
  /** null closes the dialog */
  suggestions: SpeakerNameSuggestions | null;
  currentNames: SpeakerNames;
  onClose: () => void;
  onApply: (accepted: SpeakerNames) => Promise<void>;
}

export function SpeakerSuggestionsDialog({ suggestions, currentNames, onClose, onApply }: SpeakerSuggestionsDialogProps) {
  const [accepted, setAccepted] = useState<Record<string, boolean>>({});
  const [isSaving, setIsSaving] = useState(false);
  const router = useRouter();
  const suggested = Object.entries(suggestions?.names ?? {});
  // Suggestions the speaker already carries need no action
  const entries = suggested
    .filter(([speaker, name]) => currentNames[speaker] !== name)
    .sort(([a], [b]) => a.localeCompare(b, undefined, { numeric: true }));

  useEffect(() => {
    setAccepted(Object.fromEntries(Object.keys(suggestions?.names ?? {}).map(speaker => [speaker, true])));
  }, [suggestions]);

  const handleApply = async () => {
    setIsSaving(true);
    try {
      await onApply(Object.fromEntries(entries.filter(([speaker]) => accepted[speaker])));
      onClose();
    } finally {
      setIsSaving(false);
    }
  };

  const openPreferences = () => {
    onClose();
    // Preferences is the default Settings tab
    router.push('/settings');
  };

  const description = !suggestions
    ? ''
    : !suggestions.calendar_enabled
      ? (
        <>
          Names come from the meeting&apos;s calendar invitees.{' '}
          <button type="button" className="text-blue-600 dark:text-blue-400 underline" onClick={openPreferences}>
            Enable &quot;Name meetings from calendar&quot; in Preferences
          </button>
        </>
      )
      : suggestions.attendees.length === 0
        ? 'No calendar event with invitees overlapped this meeting.'
        : entries.length === 0 && suggested.length > 0
          ? 'All suggested names are already applied.'
        : entries.length === 0
          ? `Invitees: ${suggestions.attendees.join(', ')}. Nobody was addressed by name clearly enough to match them.`
          : `Matched from the calendar invitees and how people address each other. Review before applying.`;

  return (
    <Dialog open={suggestions !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Suggested speaker names</DialogTitle>
          <DialogDescription className="break-words">{description}</DialogDescription>
        </DialogHeader>

        {entries.length > 0 && (
          <ul className="my-2 space-y-2">
            {entries.map(([speaker, name]) => (
              <li key={speaker}>
                {/* minmax(0,1fr) lets long names (e.g. emails) truncate instead of widening the dialog */}
                <label className="grid grid-cols-[auto_minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-3 text-sm text-foreground">
                  <input
                    type="checkbox"
                    checked={accepted[speaker] ?? false}
                    onChange={(e) => setAccepted(prev => ({ ...prev, [speaker]: e.target.checked }))}
                  />
                  <span className="truncate text-muted-foreground" title={speakerLabel(speaker, currentNames) ?? undefined}>
                    {speakerLabel(speaker, currentNames)}
                  </span>
                  <span>→</span>
                  <span className="truncate font-medium" title={name}>{name}</span>
                </label>
              </li>
            ))}
          </ul>
        )}

        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>Close</Button>
          {entries.length > 0 && (
            <Button type="button" onClick={handleApply} disabled={isSaving || !Object.values(accepted).some(Boolean)}>
              Apply
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
