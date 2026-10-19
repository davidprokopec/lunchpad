import { emptyButton, newActionId, type Action, type AudioTarget, type Button, type PadColor } from "./api";

export function muteCaption(target: AudioTarget, mic: string, output: string): string {
  return target.type === "app" ? target.app.name.slice(0, 12) : target.device.type === "named" ? target.device.label.slice(0, 12) : target.device.type === "defaultInput" ? mic : output;
}

export function muteButton({ target, caption, color, mutedColor }: { target: AudioTarget; caption: string; color: PadColor; mutedColor: PadColor }): Button {
  const action = (): Action => ({ id: newActionId(), wait: true, type: "setAudio", target: structuredClone(target), mode: "holdMute", volume: 50, volumeFrom: null });
  return {
    ...emptyButton(),
    look: { type: "text", caption, size: 16, face: "sans", color: "#ffffff" },
    color: structuredClone(color),
    activeColor: structuredClone(mutedColor),
    holdMs: 500,
    holdWait: false,
    down: [action()],
    up: [action()],
    stateLink: { provider: "system", kind: "audioMuted", scene: "", source: "", target: structuredClone(target) },
  };
}
