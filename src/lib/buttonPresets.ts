import { emptyButton, newActionId, type Action, type Button } from "./api";
import { DEFAULT_INPUT } from "./systemAudio";

export function muteButton(caption: string): Button {
  const action = (): Action => ({ id: newActionId(), wait: true, type: "setAudio", target: structuredClone(DEFAULT_INPUT), mode: "holdMute", volume: 50, volumeFrom: null });
  return {
    ...emptyButton(),
    look: { type: "text", caption, size: 16, face: "sans", color: "#ffffff" },
    color: { mode: "rgb", r: 0, g: 48, b: 12 },
    activeColor: { mode: "rgb", r: 255, g: 0, b: 0 },
    holdMs: 500,
    holdWait: false,
    down: [action()],
    up: [action()],
    stateLink: { provider: "system", kind: "audioMuted", scene: "", source: "", target: structuredClone(DEFAULT_INPUT) },
  };
}
