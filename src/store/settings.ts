import { create } from "zustand";
import { api, events, type AudioSettings, type InputUnavailable, type ObsSettings, type PushToTalkSettings, type Settings, type SlobsSettings, type WindowSettings, type HomeAssistantSettings } from "../lib/api";

interface SettingsStore {
  settings: Settings | null;
  inputProblem: InputUnavailable | null;
  /** Latest explicit intent, not a claim that persistence or native application succeeded. */
  audioDesired: AudioSettings | null;
  audioPending: boolean;
  audioError: string | null;
  init: () => Promise<() => void>;
  setPushToTalk: (config: PushToTalkSettings) => Promise<void>;
  setAudio: (change: Partial<AudioSettings>) => Promise<void>;
  setObs: (config: ObsSettings) => Promise<void>;
  setSlobs: (config: SlobsSettings) => Promise<void>;
  setHomeAssistant: (config: HomeAssistantSettings) => Promise<void>;
  setWindow: (config: WindowSettings) => Promise<void>;
  setDeveloperMode: (enabled: boolean) => Promise<void>;
  setSecret: (name: string, value: string) => Promise<void>;
  deleteSecret: (name: string) => Promise<void>;
  checkKeyboardAccess: () => Promise<void>;
  dismissInputProblem: () => void;
}

export const useSettingsStore = create<SettingsStore>((set, get) => {
  let audioQueue = Promise.resolve();
  let audioRevision = 0;
  let settingsRevision = 0;
  let pendingAudio = 0;

  // Full snapshots from unrelated commands/events may contain older audio fields.
  // Only a fenced get_settings read can replace the confirmed audio snapshot.
  const receiveSettings = (settings: Settings) => set((state) => ({
    settings: state.settings ? { ...settings, audio: state.settings.audio } : settings,
  }));

  const refreshSettings = async () => {
    const revision = ++settingsRevision;
    const audioVersion = audioRevision;
    try {
      const settings = await api.getSettings();
      if (revision !== settingsRevision || audioVersion !== audioRevision || pendingAudio > 0) return;
      set((state) => ({ settings, audioDesired: state.audioError ? state.audioDesired : settings.audio }));
    } catch (error) {
      if (revision === settingsRevision && audioVersion === audioRevision && pendingAudio === 0) {
        set((state) => ({ audioError: [state.audioError, `Settings refresh failed: ${String(error)}`].filter(Boolean).join("; ") }));
      }
      throw error;
    }
  };

  return {
    settings: null,
    inputProblem: null,
    audioDesired: null,
    audioPending: false,
    audioError: null,

    init: async () => {
      const unlisten = await Promise.all([
        events.onSettings((settings) => {
          receiveSettings(settings);
          if (!get().audioPending) void refreshSettings().catch(() => undefined);
        }),
        events.onInputUnavailable((problem) => set({ inputProblem: problem })),
      ]);
      await refreshSettings();
      return () => unlisten.forEach((fn) => fn());
    },

    setPushToTalk: async (config) => {
      const settings = await api.setPushToTalk(config);
      receiveSettings(settings);
    },

    setAudio: (change) => {
      const base = get().audioDesired ?? get().settings?.audio;
      if (!base) return Promise.reject(new Error("Audio settings have not loaded"));
      const desired = { ...base, ...change };
      const revision = ++audioRevision;
      ++settingsRevision;
      ++pendingAudio;
      set({ audioDesired: desired, audioPending: true });
      const task = audioQueue.then(async () => {
        let failed: { error: unknown } | null = null;
        try {
          receiveSettings(await api.setAudioSettings(desired));
        } catch (error) {
          failed = { error };
        }
        --pendingAudio;
        if (revision === audioRevision) set({ audioError: failed ? String(failed.error) : null });
        if (pendingAudio === 0) {
          try {
            await refreshSettings();
          } catch (error) {
            failed ??= { error };
          }
          if (pendingAudio === 0) set({ audioPending: false });
        }
        if (failed) throw failed.error;
      });
      // A rejected request is visible to its caller, but must not poison later intents.
      audioQueue = task.catch(() => undefined);
      return task;
    },

    setObs: async (config) => {
      const settings = await api.setObsSettings(config);
      receiveSettings(settings);
    },

    setSlobs: async (config) => {
      const settings = await api.setSlobsSettings(config);
      receiveSettings(settings);
    },
    setHomeAssistant: async (config) => {
      const settings = await api.setHomeAssistantSettings(config);
      receiveSettings(settings);
    },

    setWindow: async (config) => {
      const settings = await api.setWindowSettings(config);
      receiveSettings(settings);
    },

    setDeveloperMode: async (enabled) => {
      const settings = await api.setDeveloperMode(enabled);
      receiveSettings(settings);
    },

    setSecret: async (name, value) => {
      const settings = await api.setSecret(name, value);
      receiveSettings(settings);
    },

    deleteSecret: async (name) => {
      const settings = await api.deleteSecret(name);
      receiveSettings(settings);
    },

    checkKeyboardAccess: async () => {
      set({ inputProblem: null });
      await api.checkKeyboardAccess();
    },

    dismissInputProblem: () => set({ inputProblem: null }),
  };
});
