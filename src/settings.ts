// Settings dialog: General, Indexing, Search, About.
import { api, errorText, type AppInfo, type DriveRow, type Settings, type Status } from "./api";
import { RELEASES_API } from "./config";
import { button, fastModeDialog, makeDialog, para, toast } from "./dialogs";
import { newerVersion } from "./format";
import { t } from "./i18n";

export interface SettingsContext {
  settings: Settings;
  info: AppInfo;
  status: Status | null;
  onSaved(s: Settings): void;
}

function checkbox(label: string, checked: boolean): { row: HTMLLabelElement; input: HTMLInputElement } {
  const row = document.createElement("label");
  row.className = "check";
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = checked;
  const span = document.createElement("span");
  span.textContent = label;
  row.append(input, span);
  return { row, input };
}

function heading(text: string): HTMLHeadingElement {
  const h = document.createElement("h3");
  h.textContent = text;
  return h;
}

const KEY_NAMES: Record<string, string> = {
  Space: "Space",
  Enter: "Enter",
  Tab: "Tab",
  Backquote: "`",
  Minus: "-",
  Equal: "=",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: ",",
  Period: ".",
  Slash: "/",
  ArrowUp: "Up",
  ArrowDown: "Down",
  ArrowLeft: "Left",
  ArrowRight: "Right",
  Home: "Home",
  End: "End",
  PageUp: "PageUp",
  PageDown: "PageDown",
  Insert: "Insert",
};

/** Turns a key press into a shortcut string such as `Ctrl+Alt+Space`. */
function shortcutFrom(e: KeyboardEvent): string | null {
  let key: string | undefined;
  if (/^Key[A-Z]$/.test(e.code)) key = e.code.slice(3);
  else if (/^Digit[0-9]$/.test(e.code)) key = e.code.slice(5);
  else if (/^F([1-9]|1[0-9]|2[0-4])$/.test(e.code)) key = e.code;
  else key = KEY_NAMES[e.code];
  if (!key) return null;
  const mods = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Super"].filter(Boolean) as string[];
  if (mods.length === 0 && !/^F\d+$/.test(key)) return null;
  return [...mods, key].join("+");
}

export async function openSettings(ctx: SettingsContext): Promise<void> {
  const s: Settings = structuredClone(ctx.settings);
  const d = makeDialog(t("settings.title"), "settings");
  const tabs = document.createElement("div");
  tabs.className = "tabs";
  tabs.setAttribute("role", "tablist");
  const panels = document.createElement("div");
  panels.className = "panels";
  d.body.append(tabs, panels);

  const names = ["general", "indexing", "search", "about"] as const;
  const tabEls: HTMLButtonElement[] = [];
  const panelEls: HTMLDivElement[] = [];
  names.forEach((n, i) => {
    const tab = document.createElement("button");
    tab.type = "button";
    tab.className = "tab";
    tab.id = `tab-${n}`;
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-controls", `panel-${n}`);
    tab.textContent = t(`settings.tabs.${n}`);
    const panel = document.createElement("div");
    panel.className = "panel";
    panel.id = `panel-${n}`;
    panel.setAttribute("role", "tabpanel");
    panel.setAttribute("aria-labelledby", tab.id);
    tab.addEventListener("click", () => select(i));
    tabs.append(tab);
    panels.append(panel);
    tabEls.push(tab);
    panelEls.push(panel);
  });
  const select = (i: number) => {
    tabEls.forEach((tab, j) => {
      tab.setAttribute("aria-selected", String(i === j));
      tab.tabIndex = i === j ? 0 : -1;
      panelEls[j].hidden = i !== j;
    });
    tabEls[i].focus();
  };
  tabs.addEventListener("keydown", (e) => {
    const cur = tabEls.findIndex((x) => x.getAttribute("aria-selected") === "true");
    if (e.key === "ArrowRight" || e.key === "ArrowDown") select((cur + 1) % names.length);
    else if (e.key === "ArrowLeft" || e.key === "ArrowUp") select((cur + names.length - 1) % names.length);
    else return;
    e.preventDefault();
  });

  // --- General ---
  const general = panelEls[0];
  const hkLabel = document.createElement("label");
  hkLabel.className = "field";
  const hkSpan = document.createElement("span");
  hkSpan.textContent = t("settings.general.hotkey");
  const hk = document.createElement("input");
  hk.type = "text";
  hk.readOnly = true;
  hk.value = s.hotkey;
  hk.className = "hotkey";
  hkLabel.append(hkSpan, hk);
  const hkHelp = para(t("settings.general.hotkeyHelp"), "muted");
  const hkErr = para(ctx.info.hotkeyError ?? "", "error");
  hkErr.setAttribute("role", "alert");
  hkErr.hidden = !ctx.info.hotkeyError;
  hk.addEventListener("focus", () => (hk.placeholder = t("settings.general.hotkeyRecording")));
  hk.addEventListener("blur", () => (hk.placeholder = ""));
  hk.addEventListener("keydown", (e) => {
    if (e.key === "Tab") return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Backspace" || e.key === "Delete") {
      hk.value = "";
      return;
    }
    if (e.key === "Escape") {
      hk.value = s.hotkey;
      return;
    }
    const combo = shortcutFrom(e);
    if (combo) hk.value = combo;
  });
  const tray = checkbox(t("settings.general.tray"), s.trayIcon);
  const closeTray = checkbox(t("settings.general.closeToTray"), s.closeToTray);
  const autostart = checkbox(t("settings.general.startWithWindows"), s.startWithWindows);
  const themeLabel = document.createElement("label");
  themeLabel.className = "field";
  const themeSpan = document.createElement("span");
  themeSpan.textContent = t("settings.general.theme");
  const theme = document.createElement("select");
  for (const [v, k] of [["system", "themeSystem"], ["light", "themeLight"], ["dark", "themeDark"]]) {
    const o = document.createElement("option");
    o.value = v;
    o.textContent = t(`settings.general.${k}`);
    theme.append(o);
  }
  theme.value = s.theme;
  themeLabel.append(themeSpan, theme);
  general.append(hkLabel, hkHelp, hkErr, tray.row, closeTray.row, autostart.row, themeLabel);

  // --- Indexing ---
  const indexing = panelEls[1];
  indexing.append(heading(t("settings.indexing.drives")), para(t("settings.indexing.drivesHelp"), "muted"));
  const driveList = document.createElement("div");
  driveList.className = "drives";
  indexing.append(driveList);
  const driveBoxes = new Map<string, HTMLInputElement>();
  let drives: DriveRow[] = [];
  try {
    drives = await api.listDrives();
  } catch (e) {
    driveList.append(para(errorText(e), "error"));
  }
  for (const dr of drives) {
    const kind = t(`settings.indexing.kind${dr.kind[0].toUpperCase()}${dr.kind.slice(1)}`);
    const extra = [dr.volumeName, dr.fileSystem].filter(Boolean).join(", ");
    const box = checkbox(`${dr.label}  ${kind}${extra ? ` (${extra})` : ""}`, dr.enabled);
    driveBoxes.set(dr.label, box.input);
    driveList.append(box.row);
  }

  indexing.append(heading(t("settings.indexing.mode")));
  const modeText = para("");
  const modeBtn = button("");
  const renderMode = () => {
    const capable = drives.some((x) => x.fastCapable && x.enabled);
    if (s.fastMode) {
      modeText.textContent = t("settings.indexing.modeFast");
      modeBtn.textContent = t("settings.indexing.disableFast");
      modeBtn.disabled = false;
    } else {
      modeText.textContent = capable ? t("settings.indexing.modeStandard") : t("settings.indexing.fastUnsupported");
      modeBtn.textContent = t("settings.indexing.enableFast");
      modeBtn.disabled = !capable;
    }
  };
  renderMode();
  modeBtn.addEventListener("click", async () => {
    if (s.fastMode) {
      await api.disableFastMode();
      s.fastMode = false;
      ctx.settings.fastMode = false;
    } else {
      if (!(await fastModeDialog())) return;
      const r = await api.enableFastMode();
      if (r === "unsupported") {
        toast(t("settings.indexing.fastUnsupported"), "error");
        return;
      }
      s.fastMode = true;
      ctx.settings.fastMode = true;
    }
    renderMode();
  });
  indexing.append(modeText, modeBtn);

  indexing.append(heading(t("settings.indexing.exclusions")), para(t("settings.indexing.exclusionsHelp"), "muted"));
  const excl = document.createElement("textarea");
  excl.rows = 5;
  excl.spellcheck = false;
  excl.value = s.exclusions.join("\n");
  excl.setAttribute("aria-label", t("settings.indexing.exclusions"));
  indexing.append(excl);
  const hidden = checkbox(t("settings.indexing.hidden"), s.includeHidden);
  const system = checkbox(t("settings.indexing.system"), s.includeSystem);
  indexing.append(hidden.row, system.row);
  const actions = document.createElement("div");
  actions.className = "row-actions";
  const paused = !!ctx.status?.paused;
  const pauseBox = checkbox(t("settings.indexing.pause"), paused);
  pauseBox.input.addEventListener("change", () => void api.setPaused(pauseBox.input.checked));
  const rebuild = button(t("settings.indexing.rebuild"));
  rebuild.title = t("settings.indexing.rebuildHelp");
  rebuild.addEventListener("click", async () => {
    await api.rebuildIndex();
    toast(t("settings.indexing.rebuildStarted"));
  });
  actions.append(pauseBox.row, rebuild);
  indexing.append(actions);

  // --- Search ---
  const search = panelEls[2];
  search.append(heading(t("settings.search.defaults")));
  const mc = checkbox(t("settings.search.matchCase"), s.matchCase);
  const mp = checkbox(t("settings.search.matchPath"), s.matchPath);
  const ww = checkbox(t("settings.search.wholeWord"), s.wholeWord);
  const md = checkbox(t("settings.search.diacritics"), s.matchDiacritics);
  const limLabel = document.createElement("label");
  limLabel.className = "field";
  const limSpan = document.createElement("span");
  limSpan.textContent = t("settings.search.limit");
  const lim = document.createElement("input");
  lim.type = "number";
  lim.min = "0";
  lim.step = "1000";
  lim.value = String(s.resultLimit);
  limLabel.append(limSpan, lim);
  search.append(mc.row, mp.row, ww.row, md.row, limLabel, para(t("settings.search.limitHelp"), "muted"));

  // --- About ---
  const about = panelEls[3];
  about.append(heading(`FindIt ${t("settings.about.version", { version: ctx.info.version })}`));
  about.append(para(t("settings.about.privacy")));
  const folderRow = (labelKey: string, path: string, which: "data" | "logs") => {
    const row = document.createElement("div");
    row.className = "folder-row";
    const l = document.createElement("div");
    const strong = document.createElement("strong");
    strong.textContent = t(labelKey);
    const code = document.createElement("code");
    code.textContent = path;
    l.append(strong, document.createElement("br"), code);
    const b = button(t("settings.about.open"));
    b.addEventListener("click", () => api.openFolder(which).catch((e) => toast(errorText(e), "error")));
    row.append(l, b);
    return row;
  };
  about.append(folderRow("settings.about.dataFolder", ctx.info.dataDir, "data"), folderRow("settings.about.logFolder", ctx.info.logDir, "logs"));
  if (ctx.info.portable) about.append(para(t("settings.about.portable"), "muted"));
  about.append(para(t("settings.about.memory", { mb: ctx.info.memoryMb.toFixed(0) }), "muted"));
  const upd = button(t("updates.check"));
  const updResult = document.createElement("div");
  updResult.className = "update-result";
  updResult.setAttribute("aria-live", "polite");
  upd.addEventListener("click", async () => {
    upd.disabled = true;
    updResult.replaceChildren(para(t("updates.checking")));
    try {
      const res = await fetch(RELEASES_API, { headers: { Accept: "application/vnd.github+json" }, cache: "no-store" });
      if (!res.ok) throw new Error(String(res.status));
      const json = (await res.json()) as { tag_name?: string; html_url?: string };
      const latest = json.tag_name ?? "";
      if (latest && newerVersion(latest, ctx.info.version)) {
        const link = button(t("updates.download"), "primary");
        const url = json.html_url ?? "";
        link.addEventListener("click", () => api.openExternal(url).catch((e) => toast(errorText(e), "error")));
        updResult.replaceChildren(para(t("updates.available", { version: latest.replace(/^v/i, "") })), link);
      } else {
        updResult.replaceChildren(para(t("updates.upToDate")));
      }
    } catch {
      updResult.replaceChildren(para(t("updates.failed"), "error"));
    } finally {
      upd.disabled = false;
    }
  });
  about.append(upd, updResult, para(t("settings.about.licenses"), "muted"));

  // --- Footer ---
  const cancel = button(t("dialogs.cancel"));
  const save = button(t("settings.save"), "primary");
  d.footer.append(cancel, save);
  cancel.addEventListener("click", () => d.close());
  save.addEventListener("click", async () => {
    s.hotkey = hk.value.trim();
    s.trayIcon = tray.input.checked;
    s.closeToTray = closeTray.input.checked;
    s.startWithWindows = autostart.input.checked;
    s.theme = theme.value as Settings["theme"];
    for (const [label, box] of driveBoxes) s.drives[label] = box.checked;
    s.exclusions = excl.value
      .split(/\r?\n/)
      .map((x) => x.trim())
      .filter(Boolean);
    s.includeHidden = hidden.input.checked;
    s.includeSystem = system.input.checked;
    s.matchCase = mc.input.checked;
    s.matchPath = mp.input.checked;
    s.wholeWord = ww.input.checked;
    s.matchDiacritics = md.input.checked;
    s.resultLimit = Math.max(0, Math.floor(Number(lim.value) || 0));
    save.disabled = true;
    try {
      const r = await api.saveSettings(s);
      ctx.onSaved(r.settings);
      if (r.hotkeyError) {
        hkErr.textContent = r.hotkeyError;
        hkErr.hidden = false;
        select(0);
        hk.focus();
        return;
      }
      toast(t("settings.saved"));
      d.close();
    } catch (e) {
      toast(errorText(e), "error");
    } finally {
      save.disabled = false;
    }
  });

  d.dlg.showModal();
  select(0);
}
