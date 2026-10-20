import type { AudioStatus, ObsState, SlobsState, StateLink, WaveLinkState } from "./api";
import { audioControllable, audioState } from "./systemAudio";

/**
 * Whether a button's "active while" link currently holds. Mirrors
 * `StateLink::is_active` in src-tauri/src/live.rs, which drives the LEDs.
 */
export function isLinkActive(link: StateLink | null | undefined, obs: ObsState | null, slobs: SlobsState | null, audio: AudioStatus | null = null, waveLink: WaveLinkState | null = null): boolean {
  if (!link) return false;
  if (link.provider === "waveLink") {
    if (waveLink?.status !== "ready" || link.target?.type !== "waveLink") return false;
    if (link.kind === "audioMuted") return audioControllable(link.target, audio, "mute", waveLink) && audioState(link.target, audio, waveLink)?.muted === true;
    const target = link.target.target;
    if (target.type !== "output") return false;
    const output = waveLink.outputs.find((item) => item.outputDeviceId === target.outputDeviceId && item.outputId === target.outputId);
    if (!output) return false;
    if (link.kind === "waveLinkMainOutput") return output.canSelectMain && waveLink.mainOutput?.outputDeviceId === target.outputDeviceId && waveLink.mainOutput.outputId === target.outputId;
    return link.kind === "waveLinkMonitoredMix" && output.canSetMix && !!link.waveLinkMixId && output.mixId === link.waveLinkMixId;
  }
  if (link.provider === "system") return link.kind === "audioMuted" && !!link.target && audioControllable(link.target, audio, "mute") && !!audioState(link.target, audio)?.muted;
  const p =
    link.provider === "obs"
      ? obs && {
          connected: obs.connected,
          scene: obs.currentScene,
          visible: obs.visible,
          muted: obs.muted,
          streaming: obs.streaming,
          recording: obs.recording,
          replay: obs.replayBuffer,
        }
      : slobs && {
          connected: slobs.connected,
          scene: slobs.currentScene,
          visible: slobs.visible,
          muted: slobs.muted,
          streaming: slobs.streaming !== "offline",
          recording: slobs.recording !== "offline",
          replay: slobs.replayBuffer !== "offline",
        };
  if (!p || !p.connected) return false;
  switch (link.kind) {
    case "scene":
      return !!link.scene && p.scene === link.scene;
    case "sourceVisible": {
      const scene = link.scene || p.scene;
      return !!scene && p.visible.some(([s, src]) => s === scene && src === link.source);
    }
    case "sourceMuted":
      return p.muted.includes(link.source);
    case "streaming":
      return p.streaming;
    case "recording":
      return p.recording;
    case "replayBuffer":
      return p.replay;
    default:
      return false;
  }
}
