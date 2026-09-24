"use client";

import { useEffect, useState } from 'react';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';

interface SpeakerRenameDialogProps {
  /** Label currently shown for the speaker; null closes the dialog */
  currentLabel: string | null;
  onClose: () => void;
  /** An empty name restores the default label */
  onSave: (name: string) => Promise<void>;
}

export function SpeakerRenameDialog({ currentLabel, onClose, onSave }: SpeakerRenameDialogProps) {
  const [name, setName] = useState('');
  const [isSaving, setIsSaving] = useState(false);

  useEffect(() => {
    setName(currentLabel ?? '');
  }, [currentLabel]);

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    setIsSaving(true);
    try {
      await onSave(name);
      onClose();
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <Dialog open={currentLabel !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <form onSubmit={handleSubmit}>
          <DialogHeader>
            <DialogTitle>Rename speaker</DialogTitle>
            <DialogDescription>
              Applies to the whole meeting, including exports and new summaries. Leave it empty to restore the default.
            </DialogDescription>
          </DialogHeader>
          <Input
            className="my-4"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder={currentLabel ?? ''}
            autoFocus
          />
          <DialogFooter>
            <Button type="button" variant="outline" onClick={onClose}>Cancel</Button>
            <Button type="submit" disabled={isSaving}>Save</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
