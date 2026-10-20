import { open } from "@tauri-apps/plugin-dialog";
import { AnimatePresence, motion } from "framer-motion";
import { useCallback, useEffect, useRef, useState } from "react";
import { useShallow } from "zustand/react/shallow";
import { useTranslation } from "react-i18next";
import { api, emptyButton, type Button, type Layout, type Look, type Page } from "../lib/api";
import { FACES } from "../lib/colors";
import { cornersOf } from "../lib/grid";
import { DESCRIPTION_VARIABLES } from "../lib/placeholders";
import { useDeviceStore } from "../store/device";
import { useProfileStore } from "../store/profile";
import { useVariablesStore } from "../store/variables";
import { ActionsTab } from "./actions/ActionsTab";
import { ColorField } from "./ColorField";
import { PadFace } from "./PadFace";
import { CodeField } from "./CodeField";
import { RgbField } from "./RgbField";
import { Select } from "./Select";
import { Slider } from "./Slider";
import { ConfirmPrompt } from "./ConfirmPrompt";
import { Button as UiButton, IconClose, Segmented, Toggle } from "./ui";
import { Tooltip } from "./Tooltip";
import { StateLinkField } from "./StateLinkField";
import { muteButton, muteCaption } from "../lib/buttonPresets";
import { audioActionConfigured, audioTargetConfigured, DEFAULT_INPUT } from "../lib/systemAudio";
import { useMediaStore } from "../store/media";
import { isLinkActive } from "../lib/stateLink";
import { AudioTargetField } from "./AudioTargetField";

const NO_PAGES: Page[] = [];

/** Modal editor for one pad: look, colours and actions. */
export function ButtonEditor() {
  const { t } = useTranslation();
  const target = useProfileStore((s) => s.editor);
  const closeEditor = useProfileStore((s) => s.closeEditor);
  const buttonAt = useProfileStore((s) => s.buttonAt);
  const saveButton = useProfileStore((s) => s.saveButton);
  const clearButton = useProfileStore((s) => s.clearButton);
  const layout = useDeviceStore((s) => s.layout);
  const limited = layout?.limitedColor ?? false;
  const pages = useProfileStore((s) => s.profile?.pages) ?? NO_PAGES;
  const existing = target ? buttonAt(target.x, target.y) : null;
  // Clicking beside the dialog or pressing Escape closes it at once, unless the button has unsaved changes: then it asks.
  const [dirty, setDirty] = useState(false);
  const [confirming, setConfirming] = useState(false);
  useEffect(() => {
    setDirty(false);
    setConfirming(false);
  }, [target]);
  const requestClose = useCallback(() => {
    if (confirming) return;
    if (dirty) setConfirming(true);
    else closeEditor();
  }, [confirming, dirty, closeEditor]);

  return (
    <AnimatePresence>
      {target && (
        <motion.div
          key="editor"
          className="absolute inset-0 z-30 flex items-center justify-center bg-stage-950/70 p-6"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          onClick={requestClose}
        >
          <motion.div
            role="dialog"
            aria-label={t("editor.titleEdit")}
            initial={{ opacity: 0, scale: 0.97, y: 8 }}
            animate={{ opacity: 1, scale: 1, y: 0 }}
            exit={{ opacity: 0, scale: 0.97, y: 8 }}
            transition={{ duration: 0.18, ease: "easeOut" }}
            onClick={(e) => e.stopPropagation()}
            className="relative flex h-full max-h-[720px] w-full max-w-4xl flex-col rounded-2xl border border-stage-700 bg-stage-900 shadow-2xl"
          >
            <EditorForm
              key={`${target.pageId}:${target.x}:${target.y}`}
              pageId={target.pageId}
              x={target.x}
              y={target.y}
              initial={existing ? stripCoords(existing) : null}
              preset={target.preset}
              limited={limited}
              pages={pages}
              layout={layout}
              onCancel={requestClose}
              onDirtyChange={setDirty}
              onSave={(b) => void saveButton(target.x, target.y, b).then(closeEditor)}
              onRemove={existing ? () => void clearButton(target.x, target.y).then(closeEditor) : undefined}
            />
            <ConfirmPrompt
              open={confirming}
              title={t("editor.discardTitle")}
              body={t("editor.discardBody")}
              confirmLabel={t("editor.discard")}
              onConfirm={() => {
                setConfirming(false);
                closeEditor();
              }}
              onCancel={() => setConfirming(false)}
            />
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}

function stripCoords(b: Button & { x?: number; y?: number }): Button {
  const { x: _x, y: _y, ...rest } = b;
  return structuredClone(rest);
}

interface FormProps {
  preset?: "mute";
  pageId: string;
  x: number;
  y: number;
  initial: Button | null;
  limited: boolean;
  pages: Page[];
  layout: Layout | null;
  onCancel: () => void;
  /** Told whenever the form starts or stops differing from what is saved. */
  onDirtyChange?: (dirty: boolean) => void;
  onSave: (b: Button) => void;
  onRemove?: () => void;
}

type Tab = "appearance" | "actions";

function EditorForm({ pageId, x, y, initial, preset, limited, pages, layout, onCancel, onDirtyChange, onSave, onRemove }: FormProps) {
  const { t } = useTranslation();
  // What a description may show: the shared variables and the provided values that exist outside a run.
  const descriptionNames = useVariablesStore(useShallow((s) => [...DESCRIPTION_VARIABLES, ...Object.keys(s.globals)].sort()));
  const newMuteButton = () => muteButton({ target: DEFAULT_INPUT, caption: t("audio.micCaption"), color: { mode: "rgb", r: 0, g: 48, b: 12 }, mutedColor: { mode: "rgb", r: 255, g: 0, b: 0 } });
  const [button, setButton] = useState<Button>(() => initial ?? (preset === "mute" ? newMuteButton() : randomNewButton()));
  const [tab, setTab] = useState<Tab>("appearance");
  const muteSettingsRef = useRef<HTMLElement>(null);
  const [previewActive, setPreviewActive] = useState(false);
  const linked = useMediaStore((s) => isLinkActive(button.stateLink, s.obs, s.slobs, s.systemAudio, s.waveLink, s.fineTune));
  const pad = layout?.pads.find((p) => p.x === x && p.y === y);
  const ledKind = pad?.led ?? "rgb";
  /** A tap-only button (Arp, Scale): the device reports no release, so only the pressed list. */
  const momentary = pad?.momentary ?? false;
  const [imageError, setImageError] = useState<string | null>(null);
  const dirty = JSON.stringify(button) !== JSON.stringify(initial);
  const mutePreset = button.down.length === 1 && button.up.length === 1 && button.hold.length === 0
    && button.down[0].type === "setAudio" && button.down[0].mode === "holdMute"
    && button.up[0].type === "setAudio" && button.up[0].mode === "holdMute"
    && (button.stateLink?.provider === "system" || button.stateLink?.provider === "waveLink" || button.stateLink?.provider === "fineTune") && button.stateLink.kind === "audioMuted";
  const valid = [...button.down, ...button.up, ...button.hold].every(audioActionConfigured)
    && (!button.stateLink?.target || audioTargetConfigured(button.stateLink.target))
    && (button.stateLink?.provider !== "fineTune" || button.stateLink.target?.type === "fineTune" && (
      button.stateLink.kind === "audioMuted" ? button.stateLink.target.target.type === "app"
        : button.stateLink.kind === "fineTuneDefaultOutput" ? button.stateLink.target.target.type === "output"
        : (button.stateLink.kind === "fineTunePreferredInput" || button.stateLink.kind === "fineTuneDefaultInput") && button.stateLink.target.target.type === "input"));
  useEffect(() => onDirtyChange?.(dirty), [dirty, onDirtyChange]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && valid) onSave(button);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [button, onCancel, onSave, valid]);

  const setLook = (look: Look) => setButton({ ...button, look });
  const textLook = button.look.type === "text" ? button.look : null;
  const imageLook = button.look.type === "image" ? button.look : null;

  const pickImage = async () => {
    setImageError(null);
    const path = await open({
      multiple: false,
      directory: false,
      filters: [{ name: t("settings.fileImages"), extensions: ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"] }],
    });
    if (typeof path !== "string") return;
    try {
      const uri = await api.readImageDataUri(path);
      setLook({ type: "image", uri });
    } catch (e) {
      setImageError(String(e));
    }
  };

  return (
    <>
      <header className="flex items-center justify-between border-b border-stage-800 px-5 py-3">
        <div className="flex items-center gap-4">
          <h2 className="text-sm font-semibold text-stage-100">
            {mutePreset ? t(initial ? "audio.editMute" : "audio.setupMute") : initial ? t("editor.titleEdit") : t("editor.titleNew")}
            <span className="ml-2 font-normal text-stage-400">{t("editor.position", { column: x + 1, row: y + 1 })}</span>
          </h2>
          <Segmented
            value={tab}
            options={[
              { value: "appearance", label: t("editor.tabAppearance") },
              { value: "actions", label: `${t("editor.tabActions")}${button.down.length + button.up.length ? ` · ${button.down.length + button.up.length}` : ""}` },
            ]}
            onChange={setTab}
          />
        </div>
        <button onClick={onCancel} aria-label={t("common.close")} className="rounded-md p-1 text-stage-400 hover:bg-stage-800 hover:text-stage-100">
          <IconClose />
        </button>
      </header>

      {/* minmax(0, 1fr): a plain 1fr column grows to its content's min-content width, so one long
          unbreakable value (a URL in a row summary, say) would widen the dialog and add a horizontal
          scrollbar instead of being truncated. */}
      <div className="grid min-h-0 flex-1 grid-cols-[200px_minmax(0,1fr)] gap-6 overflow-y-auto p-5">
        {/* Preview */}
        <div className="flex flex-col items-center gap-3">
          <div
            className="h-40 w-40 rounded-[14%] bg-stage-800 p-2 shadow-[inset_0_1px_0_rgba(255,255,255,0.06)]"
            onPointerDown={() => setPreviewActive(true)}
            onPointerUp={() => setPreviewActive(false)}
            onPointerLeave={() => setPreviewActive(false)}
          >
            <PadFace button={button} cell={144} active={previewActive || linked} corners={cornersOf(pad?.shape)} limited={limited} noLed={ledKind === "none"} mask={pad?.mask ?? false} symbol={pad?.label ?? null} />
          </div>
          <p className="text-center text-xs text-stage-400">{t("editor.previewHint")}</p>
          {!initial && !momentary && !mutePreset && <UiButton size="sm" onClick={() => { setButton(newMuteButton()); setTab("appearance"); }}>{t("audio.mutePreset")}</UiButton>}
          {mutePreset && <UiButton size="sm" onClick={() => { setTab("appearance"); requestAnimationFrame(() => muteSettingsRef.current?.scrollIntoView({ block: "start" })); }}>{t("audio.muteSettings")}</UiButton>}
        </div>

        {/* Form */}
        {tab === "actions" ? (
          <div className="flex flex-col gap-3">
            {momentary && <p className="text-xs text-stage-500">{t("editor.tapOnly")}</p>}
            <ActionsTab button={button} onChange={setButton} pages={pages} layout={layout} lists={momentary ? ["down"] : undefined} />
          </div>
        ) : (
        <div className="flex flex-col gap-6">
          {mutePreset && <section ref={muteSettingsRef} className="flex flex-col gap-3">
            <h3 className="text-sm font-medium text-stage-100">{t("audio.mutePreset")}</h3>
            <p className="text-xs text-stage-400">{t("audio.setupHint")}</p>
            <h4 className="text-xs font-medium text-stage-200">{t("audio.whatMutes")}</h4>
            <AudioTargetField control="mute" value={button.stateLink?.target} onChange={(target) => {
              if (!target || !button.stateLink) return;
              const caption = (value: typeof target) => muteCaption(value, t("audio.micCaption"), t("audio.outputCaption"));
              const look = button.look.type === "text" && button.stateLink.target && button.look.caption === caption(button.stateLink.target) ? { ...button.look, caption: caption(target) } : button.look;
              setButton({ ...button, look, down: button.down.map((a) => a.type === "setAudio" ? { ...a, target } : a), up: button.up.map((a) => a.type === "setAudio" ? { ...a, target } : a), stateLink: { ...button.stateLink, target, provider: target.type === "fineTune" ? "fineTune" : target.type === "waveLink" ? "waveLink" : "system" } });
            }} />
            {textLook && <label className="flex flex-col gap-1 text-xs text-stage-400">
              {t("editor.caption")}
              <input aria-label={t("editor.caption")} value={textLook.caption} onChange={(e) => setLook({ ...textLook, caption: e.target.value })} className={inputCls} />
            </label>}
            {ledKind !== "none" && <div className="grid grid-cols-2 gap-3">
              <ColorField label={t("audio.normalColour")} value={button.color} onChange={(color) => setButton({ ...button, color })} limited={limited} />
              <ColorField label={t("audio.mutedColour")} value={button.activeColor ?? button.color} onChange={(activeColor) => setButton({ ...button, activeColor })} limited={limited} />
            </div>}
            <p className="text-xs text-stage-500">{t("audio.presetHint", { ms: button.holdMs })}</p>
          </section>}
          <section className="flex flex-col gap-3">
            <div className="flex items-center justify-between">
              <h3 className="text-sm font-medium text-stage-100">{t("editor.label")}</h3>
              <Segmented
                value={button.look.type}
                options={[
                  { value: "text", label: t("editor.text") },
                  { value: "image", label: t("editor.image") },
                ]}
                onChange={(t) =>
                  setLook(
                    t === "text"
                      ? { type: "text", caption: "", size: 16, face: "sans", color: "#ffffff" }
                      : { type: "image", uri: "" },
                  )
                }
              />
            </div>

            {textLook ? (
              <div className="grid grid-cols-[1fr_auto] gap-3">
                {!mutePreset && <label className="flex flex-col gap-1 text-xs text-stage-400">
                  {t("editor.caption")}
                  <input
                    autoFocus
                    value={textLook.caption}
                    onChange={(e) => setLook({ ...textLook, caption: e.target.value })}
                    className={inputCls}
                  />
                </label>}
                <RgbField label={t("editor.textColour")} value={textLook.color} onChange={(color) => setLook({ ...textLook, color })} />
                <div className="flex flex-col gap-1 text-xs text-stage-400">
                  {t("editor.font")}
                  <Select
                    value={textLook.face}
                    options={[
                      ...FACES.map((f) => ({ value: f.key, label: t(`faces.${f.key}`) })),
                      ...(FACES.some((f) => f.key === textLook.face) ? [] : [{ value: textLook.face, label: textLook.face }]),
                    ]}
                    onChange={(face) => setLook({ ...textLook, face })}
                    ariaLabel={t("editor.font")}
                  />
                </div>
                <div className="flex w-44 flex-col gap-1 text-xs text-stage-400">
                  {t("editor.sizeLabel")}
                  <Slider value={textLook.size} min={6} max={48} onChange={(size) => setLook({ ...textLook, size })} ariaLabel={t("editor.sizeLabel")} format={(v) => `${v}`} />
                </div>
              </div>
            ) : (
              <div className="flex items-center gap-3">
                <UiButton onClick={() => void pickImage()}>{t("editor.chooseImage")}</UiButton>
                {imageLook?.uri && <UiButton onClick={() => setLook({ type: "image", uri: "" })}>{t("editor.removeImage")}</UiButton>}
                {imageError && <span className="text-xs text-danger">{imageError}</span>}
                {!imageLook?.uri && !imageError && <span className="text-xs text-stage-400">{t("editor.imageHint")}</span>}
              </div>
            )}
          </section>

          <section className="flex flex-col gap-3">
            <div>
              <h3 className="text-sm font-medium text-stage-100">{t("editor.description")}</h3>
              <p className="mt-1 text-xs text-stage-400">{t("editor.descriptionHint")}</p>
            </div>
            <CodeField
              language="markdown"
              rows={3}
              suggestions={descriptionNames}
              value={button.description ?? ""}
              onChange={(description) => setButton({ ...button, description })}
              placeholder={t("editor.descriptionPlaceholder")}
              ariaLabel={t("editor.description")}
              title={t("editor.description")}
            />
          </section>

          {mutePreset ? null : ledKind === "none" ? (
            <p className="text-xs text-stage-500">{t("editor.noLedHint")}</p>
          ) : (
          <section className="flex flex-col gap-3">
            <h3 className="text-sm font-medium text-stage-100">{t("editor.colour")}</h3>
            {ledKind === "white" && <p className="text-xs text-stage-500">{t("editor.whiteLedHint")}</p>}
            <div className="grid grid-cols-2 gap-3">
              <ColorField label={t("editor.colour")} value={button.color} onChange={(color) => setButton({ ...button, color })} limited={limited} />
              {button.activeColor && (
                <ColorField
                  label={t("editor.colourWhileActive")}
                  value={button.activeColor}
                  onChange={(activeColor) => setButton({ ...button, activeColor })}
                  limited={limited}
                />
              )}
            </div>
            <Toggle
              checked={button.activeColor !== null}
              onChange={(v) => setButton({ ...button, activeColor: v ? { ...button.color } : null })}
              label={t("editor.activeToggle")}
              hint={t("editor.activeHint")}
            />
          </section>
          )}

          {ledKind !== "none" && !mutePreset && (
          <section className="flex flex-col gap-3">
            <div>
              <h3 className="text-sm font-medium text-stage-100">{t("editor.linkTitle")}</h3>
              <p className="mt-0.5 text-xs text-stage-400">{t("editor.linkHint")}</p>
            </div>
            <StateLinkField
              value={button.stateLink}
              onChange={(stateLink) => setButton({ ...button, stateLink, activeColor: stateLink && !button.activeColor ? { ...button.color } : button.activeColor })}
            />
          </section>
          )}
        </div>
        )}
      </div>

      <footer className="flex items-center justify-between border-t border-stage-800 px-5 py-3">
        <div className="flex items-center gap-2">
          {onRemove && (
            <UiButton variant="danger" onClick={onRemove}>
              {t("editor.removeButton")}
            </UiButton>
          )}
          {initial && (
            <Tooltip content={t("editor.testHint")} wrap>
              <UiButton
                disabled={dirty || initial.down.length + initial.up.length + initial.hold.length === 0}
                onClick={() => void api.runButton(pageId, x, y, "tap").catch(() => undefined)}
              >
                {t("editor.test")}
              </UiButton>
            </Tooltip>
          )}
        </div>
        <div className="flex gap-2">
          <UiButton onClick={onCancel}>{t("common.cancel")}</UiButton>
          <UiButton variant="primary" disabled={!valid} onClick={() => onSave(button)}>
            {t("common.save")}
          </UiButton>
        </div>
      </footer>
    </>
  );
}

const inputCls =
  "min-h-[32px] rounded-md bg-stage-800 px-2.5 py-1.5 text-sm text-stage-100 outline-none focus:ring-1 focus:ring-accent-400";


/** New buttons get a random palette colour, like dropping a file did in legacy. */
function randomNewButton(): Button {
  const b = emptyButton();
  b.color = { mode: "palette", index: 1 + Math.floor(Math.random() * 127) };
  return b;
}
