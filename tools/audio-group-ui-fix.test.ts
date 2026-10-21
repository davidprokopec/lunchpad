import { expect, test } from "bun:test";
import { api } from "../src/lib/api";
import { newFader } from "../src/lib/fader";
import { useProfileStore } from "../src/store/profile";

test("saveFader keeps its void promise/global error contract, retains editor on rejection, closes on explicit successful retry", async () => {
  const originalSetFader = api.setFader;
  const previous = useProfileStore.getState();
  const initial = newFader(0, 0, null);
  const target = { pageId: "page", fader: initial, isNew: true };
  const draft = { ...initial, name: "Unsaved Comms", min: -12, max: 88, unit: "dB", variable: "comms", bound: { type: "device" as const, device: { type: "defaultOutput" as const } } };
  const attempts: typeof draft[] = [];
  try {
    useProfileStore.setState({ faderEditor: target, error: null });
    api.setFader = async (pageId, fader) => {
      expect(pageId).toBe("page");
      expect(fader).toEqual(draft);
      attempts.push(draft);
      if (attempts.length === 1) throw Error("Save rejected");
    };
    await expect(useProfileStore.getState().saveFader(draft)).resolves.toBeUndefined();
    expect(useProfileStore.getState().error).toBe("Error: Save rejected");
    expect(useProfileStore.getState().faderEditor).toBe(target);
    expect(useProfileStore.getState().faderEditor?.fader).toBe(initial);
    await expect(useProfileStore.getState().saveFader(draft)).resolves.toBeUndefined();
    expect(useProfileStore.getState().faderEditor).toBeNull();
    expect(attempts).toHaveLength(2);
    await expect(useProfileStore.getState().saveFader(draft)).resolves.toBeUndefined();
    expect(attempts).toHaveLength(2);
  } finally {
    api.setFader = originalSetFader;
    useProfileStore.setState({ faderEditor: previous.faderEditor, error: previous.error });
  }
});
