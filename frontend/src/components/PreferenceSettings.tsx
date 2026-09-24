"use client"

import { useEffect, useState, useRef } from "react"
import { Switch } from "./ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "./ui/select"
import { FolderOpen } from "lucide-react"
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
