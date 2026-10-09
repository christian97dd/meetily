"use client"

import { useEffect, useState, useRef } from "react"
import { Switch } from "./ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "./ui/select"
import { Textarea } from "./ui/textarea"
import { Copy, FolderOpen, RefreshCw } from "lucide-react"
import { invoke } from "@tauri-apps/api/core"
import Analytics from "@/lib/analytics"
import AnalyticsConsentSwitch from "./AnalyticsConsentSwitch"
import { useConfig, NotificationSettings } from "@/contexts/ConfigContext"
import { ThemePreference, useTheme } from "@/contexts/ThemeContext"

type MeetingDetectionMode = 'off' | 'suggest' | 'auto';

interface MeetingDetectionSettings {
  mode: MeetingDetectionMode;
}

const DETECTION_MODE_HINTS: Record<MeetingDetectionMode, string> = {
  off: 'Recording only starts when you press the record button',
  suggest: 'When a browser or meeting app (Meet, Zoom, Teams...) turns the microphone on, shows a notification and a Record button',
  auto: 'Starts recording when a meeting app turns the microphone on, and stops when it releases it',
};

function MeetingDetectionSelect() {
  const [mode, setMode] = useState<MeetingDetectionMode | null>(null);

  useEffect(() => {
    invoke<MeetingDetectionSettings>('get_meeting_detection_settings')
      .then((settings) => setMode(settings.mode))
      .catch((error) => {
        console.error('Failed to load meeting detection settings:', error);
        setMode('off');
      });
  }, []);

  const handleChange = async (value: string) => {
    const next = value as MeetingDetectionMode;
    const previous = mode;
    setMode(next);
    try {
      await invoke('set_meeting_detection_settings', { settings: { mode: next } });
    } catch (error) {
      console.error('Failed to save meeting detection settings:', error);
      setMode(previous);
    }
  };

  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <h3 className="text-lg font-semibold text-foreground mb-2">Meeting detection</h3>
        <p className="text-sm text-muted-foreground">{DETECTION_MODE_HINTS[mode ?? 'off']}</p>
      </div>
      <Select value={mode ?? undefined} onValueChange={handleChange} disabled={mode === null}>
        <SelectTrigger className="w-36">
          <SelectValue placeholder="Loading..." />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="off">Off</SelectItem>
          <SelectItem value="suggest">Suggest</SelectItem>
          <SelectItem value="auto">Auto-record</SelectItem>
        </SelectContent>
      </Select>
    </div>
  );
}

function AppearanceSelect() {
  const { preference, setPreference } = useTheme();

  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <h3 className="text-lg font-semibold text-foreground mb-2">Appearance</h3>
        <p className="text-sm text-muted-foreground">System follows the macOS light/dark setting</p>
      </div>
      <Select value={preference} onValueChange={(value) => setPreference(value as ThemePreference)}>
        <SelectTrigger className="w-36">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="light">Light</SelectItem>
          <SelectItem value="dark">Dark</SelectItem>
          <SelectItem value="system">System</SelectItem>
        </SelectContent>
      </Select>
    </div>
  );
}

function CalendarNamingSwitch() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [accessDenied, setAccessDenied] = useState(false);

  useEffect(() => {
    invoke<{ enabled: boolean }>('get_calendar_naming_settings')
      .then((settings) => setEnabled(settings.enabled))
      .catch((error) => {
        console.error('Failed to load calendar settings:', error);
        setEnabled(false);
      });
  }, []);

  const handleChange = async (next: boolean) => {
    setEnabled(next);
    try {
      // Enabling triggers the macOS calendar permission prompt the first time
      const nowEnabled = await invoke<boolean>('set_calendar_naming_enabled', { enabled: next });
      setEnabled(nowEnabled);
      setAccessDenied(next && !nowEnabled);
    } catch (error) {
      console.error('Failed to save calendar settings:', error);
      setEnabled(!next);
    }
  };

  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <h3 className="text-lg font-semibold text-foreground mb-2">Name meetings from calendar</h3>
        <p className="text-sm text-muted-foreground">
          Uses the title of the event happening when you start recording. Reads macOS Calendar, so add your Google account in System Settings → Internet Accounts
        </p>
        {accessDenied && (
          <p className="text-sm text-red-600 dark:text-red-400 mt-2">
            Calendar access was denied. Allow it in System Settings → Privacy & Security → Calendars
          </p>
        )}
      </div>
      <Switch checked={enabled ?? false} disabled={enabled === null} onCheckedChange={handleChange} />
    </div>
  );
}

function AutoSpeakerNamingSwitch() {
  const [autoApply, setAutoApply] = useState<boolean | null>(null);

  useEffect(() => {
    invoke<{ auto_apply: boolean }>('get_speaker_naming_settings')
      .then((settings) => setAutoApply(settings.auto_apply))
      .catch((error) => {
        console.error('Failed to load speaker naming settings:', error);
        setAutoApply(false);
      });
  }, []);

  const handleChange = async (next: boolean) => {
    const previous = autoApply;
    setAutoApply(next);
    try {
      await invoke('set_speaker_naming_settings', { settings: { auto_apply: next } });
    } catch (error) {
      console.error('Failed to save speaker naming settings:', error);
      setAutoApply(previous);
    }
  };

  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <h3 className="text-lg font-semibold text-foreground mb-2">Name speakers automatically</h3>
        <p className="text-sm text-muted-foreground">
          When a recording is saved, applies the suggested names from the calendar invitees without asking. Needs &quot;Name meetings from calendar&quot;.
        </p>
        {autoApply && (
          <p className="text-sm text-amber-700 dark:text-amber-300 mt-2">
            Suggestions can be wrong and are applied without review. Check the names and rename any speaker that doesn&apos;t match.
          </p>
        )}
      </div>
      <Switch checked={autoApply ?? false} disabled={autoApply === null} onCheckedChange={handleChange} />
    </div>
  );
}

interface AutoRetranscriptionSettings {
  enabled: boolean;
  model: string;
}

function AutoRetranscriptionSettingsRow() {
  const [settings, setSettings] = useState<AutoRetranscriptionSettings | null>(null);

  useEffect(() => {
    invoke<AutoRetranscriptionSettings>('get_auto_retranscription_settings')
      .then(setSettings)
      .catch((error) => {
        console.error('Failed to load retranscription settings:', error);
        setSettings({ enabled: false, model: 'large-v3' });
      });
  }, []);

  const save = async (next: AutoRetranscriptionSettings) => {
    const previous = settings;
    setSettings(next);
    try {
      await invoke('set_auto_retranscription_settings', { settings: next });
    } catch (error) {
      console.error('Failed to save retranscription settings:', error);
      setSettings(previous);
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-4">
        <div>
          <h3 className="text-lg font-semibold text-foreground mb-2">Re-transcribe after recording</h3>
          <p className="text-sm text-muted-foreground">
            When a recording ends, transcribes the full audio again with a larger Whisper model and longer context, keeping who said what. Replaces the live transcript when done; it takes a few minutes per hour of audio and uses the GPU meanwhile.
          </p>
        </div>
        <Switch
          checked={settings?.enabled ?? false}
          disabled={settings === null}
          onCheckedChange={(enabled) => settings && save({ ...settings, enabled })}
        />
      </div>
      {settings?.enabled && (
        <div className="flex items-center justify-between gap-4">
          <p className="text-sm text-muted-foreground">
            Model. Download it first in Transcription settings; if it&apos;s missing, the live model is used.
          </p>
          <Select value={settings.model} onValueChange={(model) => save({ ...settings, model })}>
            <SelectTrigger className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="large-v3">large-v3 (most accurate)</SelectItem>
              <SelectItem value="large-v3-turbo">large-v3-turbo (faster)</SelectItem>
            </SelectContent>
          </Select>
        </div>
      )}
    </div>
  );
}

function VocabularySettings() {
  const [terms, setTerms] = useState<string | null>(null);
  const [saved, setSaved] = useState<string>('');

  useEffect(() => {
    invoke<{ terms: string }>('get_vocabulary_settings')
      .then(({ terms }) => {
        setTerms(terms);
        setSaved(terms);
      })
      .catch((error) => {
        console.error('Failed to load vocabulary:', error);
        setTerms('');
      });
  }, []);

  const handleBlur = async () => {
    if (terms === null || terms === saved) return;
    try {
      await invoke('set_vocabulary_settings', { settings: { terms } });
      setSaved(terms);
    } catch (error) {
      console.error('Failed to save vocabulary:', error);
    }
  };

  return (
    <div>
      <h3 className="text-lg font-semibold text-foreground mb-2">Vocabulary</h3>
      <p className="text-sm text-muted-foreground mb-3">
        Names and terms Whisper should spell correctly, separated by commas or lines (e.g. product names, team jargon, people's names). Keep it short: only the first ~500 characters are used. Whisper only; Parakeet ignores it.
      </p>
      <Textarea
        value={terms ?? ''}
        disabled={terms === null}
        onChange={(e) => setTerms(e.target.value)}
        onBlur={handleBlur}
        rows={3}
        placeholder="Acme, Kubernetes, OKR"
      />
    </div>
  );
}

interface LocalApiSettings {
  enabled: boolean;
  allow_content: boolean;
  allow_delete: boolean;
  port: number;
  token: string;
}

function LocalApiSettingsSection() {
  const [settings, setSettings] = useState<LocalApiSettings | null>(null);

  useEffect(() => {
    invoke<LocalApiSettings>('get_local_api_settings')
      .then(setSettings)
      .catch((error) => console.error('Failed to load local API settings:', error));
  }, []);

  const update = async (changes: Partial<Pick<LocalApiSettings, 'enabled' | 'allow_content' | 'allow_delete'>>, regenerateToken = false) => {
    if (!settings) return;
    const next = { ...settings, ...changes };
    try {
      setSettings(await invoke<LocalApiSettings>('set_local_api_settings', {
        update: {
          enabled: next.enabled,
          allow_content: next.allow_content,
          allow_delete: next.allow_delete,
          regenerate_token: regenerateToken,
        },
      }));
    } catch (error) {
      console.error('Failed to save local API settings:', error);
    }
  };

  if (!settings) return null;

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-4">
        <div>
          <h3 className="text-lg font-semibold text-foreground mb-2">Local API</h3>
          <p className="text-sm text-muted-foreground">
            Lets tools on this Mac (an MCP server, scripts) list meetings, summarize, export and control recording. Only reachable from this computer and only with the token below.
          </p>
        </div>
        <Switch checked={settings.enabled} onCheckedChange={(enabled) => update({ enabled })} />
      </div>

      {settings.enabled && (
        <>
          <div className="flex items-center justify-between gap-4">
            <div>
              <div className="font-medium text-foreground">Allow reading meeting content</div>
              <p className="text-sm text-muted-foreground">
                Transcripts, summaries and search results. Off, clients can still trigger summaries and exports but never receive what was said.
              </p>
              {settings.allow_content && (
                <p className="text-sm text-amber-700 dark:text-amber-300 mt-2">
                  Anything with the token can read your meetings. If the client is an AI assistant, the content goes to its provider.
                </p>
              )}
            </div>
            <Switch checked={settings.allow_content} onCheckedChange={(allow) => update({ allow_content: allow })} />
          </div>

          <div className="flex items-center justify-between gap-4">
            <div>
              <div className="font-medium text-foreground">Allow deleting meetings</div>
              <p className="text-sm text-muted-foreground">
                Removes the meeting, its transcript, summary and recording files. It can&apos;t be undone.
              </p>
            </div>
            <Switch checked={settings.allow_delete} onCheckedChange={(allow) => update({ allow_delete: allow })} />
          </div>

          <div className="p-3 border border-border rounded-lg bg-muted/50 space-y-2 text-sm">
            <div className="text-muted-foreground">
              URL: <span className="font-mono text-foreground">http://127.0.0.1:{settings.port}/v1</span>
            </div>
            <div className="flex items-center gap-2">
              <span className="text-muted-foreground">Token:</span>
              <span className="font-mono text-foreground truncate">{settings.token.slice(0, 8)}…</span>
              <button
                type="button"
                className="text-muted-foreground hover:text-foreground"
                title="Copy token"
                onClick={() => navigator.clipboard.writeText(settings.token)}
              >
                <Copy className="w-4 h-4" />
              </button>
              <button
                type="button"
                className="text-muted-foreground hover:text-foreground"
                title="Regenerate token (disconnects current clients)"
                onClick={() => update({}, true)}
              >
                <RefreshCw className="w-4 h-4" />
              </button>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

export function PreferenceSettings() {
  const {
    notificationSettings,
    storageLocations,
    isLoadingPreferences,
    loadPreferences,
    updateNotificationSettings
  } = useConfig();

  const [notificationsEnabled, setNotificationsEnabled] = useState<boolean | null>(null);
  const [isInitialLoad, setIsInitialLoad] = useState(true);
  const [previousNotificationsEnabled, setPreviousNotificationsEnabled] = useState<boolean | null>(null);
  const hasTrackedViewRef = useRef(false);

  // Lazy load preferences on mount (only loads if not already cached)
  useEffect(() => {
    loadPreferences();
    // Reset tracking ref on mount (every tab visit)
    hasTrackedViewRef.current = false;
  }, [loadPreferences]);

  // Track preferences viewed analytics on every tab visit (once per mount)
  useEffect(() => {
    if (hasTrackedViewRef.current) return;

    const trackPreferencesViewed = async () => {
      // Wait for notification settings to be available (either from cache or after loading)
      if (notificationSettings) {
        await Analytics.track('preferences_viewed', {
          notifications_enabled: notificationSettings.notification_preferences.show_recording_started ? 'true' : 'false'
        });
        hasTrackedViewRef.current = true;
      } else if (!isLoadingPreferences) {
        // If not loading and no settings available, track with default value
        await Analytics.track('preferences_viewed', {
          notifications_enabled: 'false'
        });
        hasTrackedViewRef.current = true;
      }
    };

    trackPreferencesViewed();
  }, [notificationSettings, isLoadingPreferences]);

  // Update notificationsEnabled when notificationSettings are loaded from global state
  useEffect(() => {
    if (notificationSettings) {
      // Notification enabled means both started and stopped notifications are enabled
      const enabled =
        notificationSettings.notification_preferences.show_recording_started &&
        notificationSettings.notification_preferences.show_recording_stopped;
      setNotificationsEnabled(enabled);
      if (isInitialLoad) {
        setPreviousNotificationsEnabled(enabled);
        setIsInitialLoad(false);
      }
    } else if (!isLoadingPreferences) {
      // If not loading and no settings, use default
      setNotificationsEnabled(true);
      if (isInitialLoad) {
        setPreviousNotificationsEnabled(true);
        setIsInitialLoad(false);
      }
    }
  }, [notificationSettings, isLoadingPreferences, isInitialLoad])

  useEffect(() => {
    // Skip update on initial load or if value hasn't actually changed
    if (isInitialLoad || notificationsEnabled === null || notificationsEnabled === previousNotificationsEnabled) return;
    if (!notificationSettings) return;

    const handleUpdateNotificationSettings = async () => {
      console.log("Updating notification settings to:", notificationsEnabled);

      try {
        // Update the notification preferences
        const updatedSettings: NotificationSettings = {
          ...notificationSettings,
          notification_preferences: {
            ...notificationSettings.notification_preferences,
            show_recording_started: notificationsEnabled,
            show_recording_stopped: notificationsEnabled,
          }
        };

        console.log("Calling updateNotificationSettings with:", updatedSettings);
        await updateNotificationSettings(updatedSettings);
        setPreviousNotificationsEnabled(notificationsEnabled);
        console.log("Successfully updated notification settings to:", notificationsEnabled);

        // Track notification preference change - only fires when user manually toggles
        await Analytics.track('notification_settings_changed', {
          notifications_enabled: notificationsEnabled.toString()
        });
      } catch (error) {
        console.error('Failed to update notification settings:', error);
      }
    };

    handleUpdateNotificationSettings();
  }, [notificationsEnabled, notificationSettings, isInitialLoad, previousNotificationsEnabled, updateNotificationSettings])

  const handleOpenFolder = async (folderType: 'database' | 'models' | 'recordings') => {
    try {
      switch (folderType) {
        case 'database':
          await invoke('open_database_folder');
          break;
        case 'models':
          await invoke('open_models_folder');
          break;
        case 'recordings':
          await invoke('open_recordings_folder');
          break;
      }

      // Track storage folder access
      await Analytics.track('storage_folder_opened', {
        folder_type: folderType
      });
    } catch (error) {
      console.error(`Failed to open ${folderType} folder:`, error);
    }
  };

  // Show loading only if we're actually loading and don't have cached data
  if (isLoadingPreferences && !notificationSettings && !storageLocations) {
    return <div className="max-w-2xl mx-auto p-6">Loading Preferences...</div>
  }

  // Show loading if notificationsEnabled hasn't been determined yet
  if (notificationsEnabled === null && !isLoadingPreferences) {
    return <div className="max-w-2xl mx-auto p-6">Loading Preferences...</div>
  }

  // Ensure we have a boolean value for the Switch component
  const notificationsEnabledValue = notificationsEnabled ?? false;

  return (
    <div className="space-y-6">
      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <AppearanceSelect />
      </div>

      {/* Notifications Section */}
      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <div className="flex items-center justify-between">
          <div>
            <h3 className="text-lg font-semibold text-foreground mb-2">Notifications</h3>
            <p className="text-sm text-muted-foreground">Enable or disable notifications of start and end of meeting</p>
          </div>
          <Switch checked={notificationsEnabledValue} onCheckedChange={setNotificationsEnabled} />
        </div>
      </div>

      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <MeetingDetectionSelect />
      </div>

      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <CalendarNamingSwitch />
      </div>

      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <AutoSpeakerNamingSwitch />
      </div>

      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <AutoRetranscriptionSettingsRow />
      </div>

      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <VocabularySettings />
      </div>

      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <LocalApiSettingsSection />
      </div>

      {/* Data Storage Locations Section */}
      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <h3 className="text-lg font-semibold text-foreground mb-4">Data Storage Locations</h3>
        <p className="text-sm text-muted-foreground mb-6">
          View and access where Meetily stores your data
        </p>

        <div className="space-y-4">
          {/* Database Location */}
          {/* <div className="p-4 border rounded-lg bg-muted/50">
            <div className="font-medium mb-2">Database</div>
            <div className="text-sm text-muted-foreground mb-3 break-all font-mono text-xs">
              {storageLocations?.database || 'Loading...'}
            </div>
            <button
              onClick={() => handleOpenFolder('database')}
              className="flex items-center gap-2 px-3 py-2 text-sm border border-input rounded-md hover:bg-accent transition-colors"
            >
              <FolderOpen className="w-4 h-4" />
              Open Folder
            </button>
          </div> */}

          {/* Models Location */}
          {/* <div className="p-4 border rounded-lg bg-muted/50">
            <div className="font-medium mb-2">Whisper Models</div>
            <div className="text-sm text-muted-foreground mb-3 break-all font-mono text-xs">
              {storageLocations?.models || 'Loading...'}
            </div>
            <button
              onClick={() => handleOpenFolder('models')}
              className="flex items-center gap-2 px-3 py-2 text-sm border border-input rounded-md hover:bg-accent transition-colors"
            >
              <FolderOpen className="w-4 h-4" />
              Open Folder
            </button>
          </div> */}

          {/* Recordings Location */}
          <div className="p-4 border rounded-lg bg-muted/50">
            <div className="font-medium mb-2">Meeting Recordings</div>
            <div className="text-sm text-muted-foreground mb-3 break-all font-mono text-xs">
              {storageLocations?.recordings || 'Loading...'}
            </div>
            <button
              onClick={() => handleOpenFolder('recordings')}
              className="flex items-center gap-2 px-3 py-2 text-sm border border-input rounded-md hover:bg-accent transition-colors"
            >
              <FolderOpen className="w-4 h-4" />
              Open Folder
            </button>
          </div>
        </div>

        <div className="mt-4 p-3 bg-blue-50 dark:bg-blue-950/40 rounded-md">
          <p className="text-xs text-blue-800 dark:text-blue-300">
            <strong>Note:</strong> Database and models are stored together in your application data directory for unified management.
          </p>
        </div>
      </div>

      {/* Analytics Section */}
      <div className="bg-background rounded-lg border border-border p-6 shadow-sm">
        <AnalyticsConsentSwitch />
      </div>
    </div>
  )
}
