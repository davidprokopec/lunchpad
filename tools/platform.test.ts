import { expect, test } from "bun:test";
import { isMacOS, platformOptionAvailable } from "../src/lib/platform";

test("FineTune choices are macOS-only without filtering other integrations", () => {
  const choices = ["fineTune", "fineTuneSetAppRouting", "fineTuneSetDefaultOutput", "fineTuneSetPreferredInput", "waveLink", "setAudio", "device", "app"];
  expect(isMacOS("MacIntel")).toBe(true);
  expect(isMacOS("MACARM")).toBe(true);
  for (const platform of ["Win32", "Linux x86_64", ""]) {
    expect(isMacOS(platform)).toBe(false);
    expect(choices.filter((value) => platformOptionAvailable(value, platform))).toEqual(["waveLink", "setAudio", "device", "app"]);
  }
  expect(choices.filter((value) => platformOptionAvailable(value, "MacIntel"))).toEqual(choices);
  expect(choices).toHaveLength(8);
});
