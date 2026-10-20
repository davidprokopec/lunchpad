import type { WaveLinkState, WaveLinkTarget } from "./api";

/** Explicit tuples keep opaque parent/child IDs and target kinds separate. */
export function waveLinkKey(target: WaveLinkTarget): string {
  switch (target.type) {
    case "channel": return JSON.stringify([target.type, target.channelId]);
    case "channelMix": return JSON.stringify([target.type, target.channelId, target.mixId]);
    case "mix": return JSON.stringify([target.type, target.mixId]);
    case "output": return JSON.stringify([target.type, target.outputDeviceId, target.outputId]);
    case "input": case "inputGain": return JSON.stringify([target.type, target.inputDeviceId, target.inputId]);
  }
}

export function waveLinkControl(target: WaveLinkTarget, state: WaveLinkState | null) {
  return state?.controls.find((control) => waveLinkKey(control.target) === waveLinkKey(target)) ?? null;
}

export const waveLinkOutputKey = (deviceId: string, outputId: string) => waveLinkKey({ type: "output", outputDeviceId: deviceId, outputId });
