// FindIt main window: search box, filters, results grid, status bar.
import "./styles.css";
import { api, errorText, on, type AppInfo, type Chip, type IndexEvent, type OpenAction, type Row, type Settings, type SortColumn, type Status } from "./api";
import { confirmDialog, helpDialog, promptDialog, toast } from "./dialogs";
import { formatCount, formatSize, tzOffsetSecs } from "./format";
import { Grid } from "./grid";
import { applyI18n, t } from "./i18n";
import { closeMenu, menuOpen, showMenu } from "./menu";
import { PreviewPane } from "./preview";
import { openSettings } from "./settings";
import { openWelcome } from "./welcome";

const CHIPS: Chip[] = ["all", "files", "folders", "documents", "images", "video", "audio", "archives", "apps"];

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
const input = $<HTMLInputElement>("q");
const hintEl = $<HTMLDivElement>("hint");
const searchbox = $<HTMLDivElement>("searchbox");
const historyEl = $<HTMLDivElement>("history");
const historyBtn = $<HTMLButtonElement>("history-btn");
const stResults = $<HTMLSpanElement>("st-results");
const stIndex = $<HTMLSpanElement>("st-index");
const stSelection = $<HTMLSpanElement>("st-selection");

const state = {
  settings: null as Settings | null,
  info: null as AppInfo | null,
  status: null as Status | null,
  chip: "all" as Chip,
  sort: "relevance" as SortColumn,
  descending: false,
  matchCase: false,
  matchPath: false,
  wholeWord: false,
  matchDiacritics: false,
  searchId: 0,
  total: 0,
  shown: 0,
  elapsedMs: 0,
  limited: false,
  selected: new Set<number>(),
  anchor: -1,
  focusId: -1,
};

// ----- caches for type names and icons ---------------------------------------------------

const typeNames = new Map<string, string>();
const icons = new Map<string, string | null>();
const wantTypes = new Set<string>();
const wantIcons = new Set<string>();
let lookupTimer: number | undefined;

function rowKey(r: Row): string {
  return r.isDir ? "folder" : r.ext || "file";
}

function requestLookups(): void {
  if (lookupTimer !== undefined) return;
  lookupTimer = window.setTimeout(async () => {
    lookupTimer = undefined;
    const tk = [...wantTypes];
    const ik = [...wantIcons];
    wantTypes.clear();
    wantIcons.clear();
    try {
      const empty: Record<string, string> = {};
      const [names, urls] = await Promise.all([tk.length ? api.typeNames(tk) : empty, ik.length ? api.icons(ik) : empty]);
      tk.forEach((k) => typeNames.set(k, names[k] ?? ""));
      ik.forEach((k) => icons.set(k, urls[k] ?? null));
      grid.refreshRows();
    } catch {
      tk.forEach((k) => typeNames.set(k, ""));
      ik.forEach((k) => icons.set(k, null));
    }
  }, 30);
}

// ----- grid --------------------------------------------------------------------------------

const preview = new PreviewPane($("preview"));

const grid = new Grid($("grid"), [], {
  fetchRows: async (searchId, start, count) => {
    const r = await api.getRows(searchId, start, count);
    return r.stale ? null : r.rows;
  },
  onRowPointer: (index, row, e) => {
    if (e.button === 2) return;
    grid.viewport.focus();
    if (e.shiftKey) void setFocus(index, "extend");
    else if (e.ctrlKey) void setFocus(index, "toggle");
    else if (row) void setFocus(index, "single");
  },
  onRowActivate: (_index, row) => {
    state.selected = new Set([row.id]);
    void openSelected("open");
  },
  onContextMenu: async (index, row, e) => {
    if (row && !state.selected.has(row.id)) await setFocus(index, "single");
    openContextMenu(e.clientX, e.clientY);
  },
  onSort: (col) => {
    if (state.sort === col) {
      if (!state.descending) state.descending = true;
      else {
        state.sort = "relevance";
        state.descending = false;
      }
    } else {
      state.sort = col;
      state.descending = false;
    }
    grid.setSort(state.sort, state.descending);
    if (state.settings) {
      state.settings.sortColumn = state.sort;
      state.settings.sortDescending = state.descending;
      persistSoon();
    }
    void runSearch(false);
  },
  onColumnsChanged: (cols) => {
    if (state.settings) {
      state.settings.columns = cols.map((c) => ({ ...c }));
      persistSoon();
    }
  },
  isSelected: (id) => state.selected.has(id),
  typeName: (row) => {
    const k = rowKey(row);
    const v = typeNames.get(k);
    if (v === undefined) {
      wantTypes.add(k);
      requestLookups();
      return "";
    }
    return v;
  },
  iconUrl: (row) => {
    const k = rowKey(row);
    const v = icons.get(k);
    if (v === undefined) {
      wantIcons.add(k);
      requestLookups();
      return null;
    }
    return v;
  },
  onStale: () => void runSearch(true),
  onRendered: () => {},
});

// ----- settings persistence ------------------------------------------------------------------

let persistTimer: number | undefined;
function persistSoon(): void {
  window.clearTimeout(persistTimer);
  persistTimer = window.setTimeout(() => {
    if (state.settings) api.saveSettings(state.settings).catch(() => {});
  }, 600);
}

// ----- search ---------------------------------------------------------------------------------

let latest = 0;
let inFlight = 0;
let refreshAfterFlight = false;

function showHint(text: string | null, kind: "error" | "warn" = "error"): void {
  hintEl.hidden = !text;
  hintEl.textContent = text ?? "";
  hintEl.className = `hint ${kind}`;
  searchbox.classList.toggle("invalid", !!text && kind === "error");
  input.setAttribute("aria-invalid", String(!!text && kind === "error"));
}

async function runSearch(keepScroll: boolean): Promise<void> {
  const token = ++latest;
  inFlight++;
  try {
    await doSearch(token, keepScroll);
  } finally {
    inFlight--;
    if (inFlight === 0 && refreshAfterFlight) {
      refreshAfterFlight = false;
      scheduleRefresh(state.status?.indexing ?? false);
    }
  }
}

async function doSearch(token: number, keepScroll: boolean): Promise<void> {
  let res;
  try {
    res = await api.search({
      query: input.value,
      chip: state.chip,
      sort: state.sort,
      descending: state.descending,
      matchCase: state.matchCase,
      matchPath: state.matchPath,
      wholeWord: state.wholeWord,
      matchDiacritics: state.matchDiacritics,
      tzOffsetSecs: tzOffsetSecs(),
    });
  } catch (e) {
    if (token === latest) showHint(t("errors.generic", { message: errorText(e) }));
    return;
  }
  if (token !== latest || res.cancelled) return;
  if (res.error) {
    // Keep the previous results on screen; just explain what is wrong.
    showHint(res.error);
    return;
  }
  showHint(res.timedOut ? t("status.timedOut") : null, "warn");
  state.searchId = res.searchId;
  state.total = res.total;
  state.shown = res.shown;
  state.elapsedMs = res.elapsedMs;
  state.limited = res.limited;
  if (!keepScroll) {
    state.selected.clear();
    state.focusId = -1;
    state.anchor = -1;
  }
  grid.setResult(res.searchId, res.shown, keepScroll);
  if (keepScroll && state.focusId >= 0) {
    const idx = await api.indexOf(res.searchId, state.focusId).catch(() => null);
    if (token === latest) {
      grid.focusIndex = idx ?? -1;
      grid.refreshRows();
    }
  }
  updateResultsStatus();
  updateEmpty();
  updateSelectionStatus();
}

let inputFrame = 0;
input.addEventListener("input", () => {
  cancelAnimationFrame(inputFrame);
  inputFrame = requestAnimationFrame(() => void runSearch(false));
});

// Live updates: refresh at most 10 times a second, once a second while indexing.
let refreshTimer: number | undefined;
let lastRefresh = 0;
function scheduleRefresh(indexing: boolean): void {
  // Never cancel a search that is still running (large indexes while indexing):
  // refresh once it is done instead.
  if (inFlight > 0) {
    refreshAfterFlight = true;
    return;
  }
  if (refreshTimer !== undefined) return;
  const gap = indexing ? 1000 : 120;
  const wait = Math.max(0, lastRefresh + gap - Date.now());
  refreshTimer = window.setTimeout(() => {
    refreshTimer = undefined;
    lastRefresh = Date.now();
    if (inFlight > 0) refreshAfterFlight = true;
    else void runSearch(true);
  }, wait);
}

// ----- selection --------------------------------------------------------------------------------

type FocusMode = "single" | "extend" | "toggle" | "move";

async function setFocus(index: number, mode: FocusMode): Promise<void> {
  if (grid.total === 0) return;
  index = Math.max(0, Math.min(index, grid.total - 1));
  const row = await grid.ensureRow(index);
  if (!row) return;
  grid.focusIndex = index;
  state.focusId = row.id;
  if (mode === "single") {
    state.selected = new Set([row.id]);
    state.anchor = index;
  } else if (mode === "toggle") {
    if (state.selected.has(row.id)) state.selected.delete(row.id);
    else state.selected.add(row.id);
    state.anchor = index;
  } else if (mode === "extend") {
    const a = state.anchor < 0 ? index : state.anchor;
    const ids = await api.idsInRange(state.searchId, Math.min(a, index), Math.max(a, index) + 1);
    if (ids) state.selected = new Set(ids);
    else toast(t("errors.tooManySelected"), "error");
  }
  grid.scrollIntoView(index);
  grid.refreshRows();
  updateSelectionStatus();
  preview.show(row.isDir ? null : row.id);
}

async function selectAll(): Promise<void> {
  const ids = await api.idsInRange(state.searchId, 0, grid.total);
  if (!ids) {
    toast(t("errors.tooManySelected"), "error");
    return;
  }
  state.selected = new Set(ids);
  grid.refreshRows();
  updateSelectionStatus();
}

function selectedIds(): number[] {
  if (state.selected.size) return [...state.selected];
  const r = grid.rowAt(grid.focusIndex);
  return r ? [r.id] : [];
}

// ----- actions ------------------------------------------------------------------------------------

async function openSelected(action: OpenAction): Promise<void> {
  let ids = selectedIds();
  if (ids.length === 0 && grid.total > 0) {
    const first = await grid.ensureRow(0);
    if (first) ids = [first.id];
  }
  if (ids.length === 0) return;
  try {
    await api.openItems(ids, action);
    if (input.value.trim() && state.settings) state.settings.history = await api.rememberQuery(input.value);
  } catch (e) {
    toast(errorText(e), "error");
  }
}

async function copySelected(what: "files" | "paths" | "names"): Promise<void> {
  const ids = selectedIds();
  if (!ids.length) return;
  try {
    await api.copyItems(ids, what);
    toast(t(what === "files" ? "dialogs.copied" : what === "paths" ? "dialogs.copiedPaths" : "dialogs.copiedNames"));
  } catch (e) {
    toast(errorText(e), "error");
  }
}

async function renameFocused(): Promise<void> {
  const row = grid.rowAt(grid.focusIndex);
  if (!row || row.deleted) return;
  const dot = row.isDir ? -1 : row.name.lastIndexOf(".");
  await promptDialog({
    title: t("dialogs.rename.title"),
    label: t("dialogs.rename.label"),
    value: row.name,
    confirm: t("dialogs.rename.confirm"),
    select: [0, dot > 0 ? dot : row.name.length],
    submit: async (value) => {
      if (value === row.name) return null;
      try {
        await api.renameItem(row.id, value);
        void runSearch(true);
        return null;
      } catch (e) {
        return errorText(e);
      }
    },
  });
  grid.viewport.focus();
}

async function deleteSelected(): Promise<void> {
  const ids = selectedIds();
  if (!ids.length) return;
  const one = ids.length === 1 ? grid.rowAt(grid.focusIndex) : undefined;
  const ok = await confirmDialog({
    title: t("dialogs.delete.title"),
    message: one && one.id === ids[0] ? t("dialogs.delete.one", { name: one.name }) : t("dialogs.delete.many", { count: formatCount(ids.length) }),
    note: t("dialogs.delete.note"),
    confirm: t("dialogs.delete.confirm"),
    danger: true,
  });
  grid.viewport.focus();
  if (!ok) return;
  try {
    const r = await api.deleteItems(ids);
    if (r.deleted > 0) toast(t("dialogs.delete.done", { count: formatCount(r.deleted) }));
    state.selected.clear();
    void runSearch(true);
  } catch (e) {
    toast(errorText(e), "error");
  }
}

function openContextMenu(x: number, y: number): void {
  const hasSel = selectedIds().length > 0;
  const many = selectedIds().length > 1;
  showMenu(
    x,
    y,
    t("menu.label"),
    [
      { id: "open", label: t("menu.open"), shortcut: "Enter", disabled: !hasSel },
      { id: "openWith", label: t("menu.openWith"), disabled: !hasSel || many },
      { id: "reveal", label: t("menu.reveal"), shortcut: "Ctrl+Enter", disabled: !hasSel },
      { id: "copy", label: t("menu.copy"), shortcut: "Ctrl+C", separatorBefore: true, disabled: !hasSel },
      { id: "copyPath", label: t("menu.copyPath"), shortcut: "Ctrl+Shift+C", disabled: !hasSel },
      { id: "copyName", label: t("menu.copyName"), disabled: !hasSel },
      { id: "rename", label: t("menu.rename"), shortcut: "F2", separatorBefore: true, disabled: !hasSel || many },
      { id: "delete", label: t("menu.delete"), shortcut: "Del", disabled: !hasSel },
      { id: "properties", label: t("menu.properties"), shortcut: "Alt+Enter", separatorBefore: true, disabled: !hasSel || many },
    ],
    (id) => {
      switch (id) {
        case "open":
        case "openWith":
        case "reveal":
        case "properties":
          void openSelected(id);
          break;
        case "copy":
          void copySelected("files");
          break;
        case "copyPath":
          void copySelected("paths");
          break;
        case "copyName":
          void copySelected("names");
          break;
        case "rename":
          void renameFocused();
          break;
        case "delete":
          void deleteSelected();
          break;
      }
    },
    grid.viewport,
  );
}

// ----- status bar ------------------------------------------------------------------------------------

function updateResultsStatus(): void {
  let text: string;
  if (state.limited) text = t("status.resultsLimited", { shown: formatCount(state.shown), count: formatCount(state.total) });
  else if (state.total === 1) text = t("status.result");
  else text = t("status.results", { count: formatCount(state.total) });
  const partial = state.status?.indexing && !state.status?.paused;
  stResults.textContent = partial ? `${text} · ${t("status.partial")}` : text;
  stResults.classList.toggle("partial", !!partial);
  stResults.title = t("status.elapsed", { ms: state.elapsedMs.toFixed(1) });
}

function updateIndexStatus(): void {
  const st = state.status;
  if (!st) return;
  const parts: string[] = [];
  if (st.loading) parts.push(t("status.loading"));
  else if (!state.settings?.firstRunDone) parts.push(t("status.notStarted"));
  else {
    const busy = st.volumes.find((v) => v.phase !== "ready" && v.phase !== "error");
    if (st.paused) parts.push(t("status.paused"));
    else if (busy) {
      const p = { drive: busy.label, count: formatCount(busy.count) };
      const key = { scanning: "status.scanning", enumerating: "status.enumerating", filling: "status.filling", waiting: "status.waiting" }[busy.phase as string] ?? "status.scanning";
      parts.push(t(key, p));
    } else parts.push(t("status.ready", { count: formatCount(st.total) }));
  }
  if (st.fastMode === "on") parts.push(t("status.fastOn"));
  else if (st.fastMode === "starting") parts.push(t("status.fastStarting"));
  const note = st.message ?? st.volumes.find((v) => v.message)?.message ?? null;
  if (note) parts.push(note);
  stIndex.textContent = parts.join(" · ");
  stIndex.title = st.denied > 0 ? t("status.denied", { count: formatCount(st.denied) }) : "";
  stIndex.classList.toggle("busy", st.indexing && !st.paused);
}

let selTimer: number | undefined;
function updateSelectionStatus(): void {
  window.clearTimeout(selTimer);
  const ids = [...state.selected];
  if (ids.length === 0) {
    stSelection.textContent = "";
    return;
  }
  selTimer = window.setTimeout(async () => {
    try {
      const info = await api.selectionInfo(ids.slice(0, 100_000));
      const count = formatCount(ids.length);
      stSelection.textContent = info.files > 0 ? t("status.selectedSize", { count, size: formatSize(info.totalSize) }) : t("status.selected", { count });
    } catch {
      stSelection.textContent = "";
    }
  }, 80);
}

function updateEmpty(): void {
  if (state.total > 0) {
    grid.setEmptyMessage(null);
    return;
  }
  const st = state.status;
  if (st?.loading) grid.setEmptyMessage([t("empty.loading")]);
  else if (!state.settings?.firstRunDone) grid.setEmptyMessage([t("empty.notStarted")]);
  else if (st?.indexing) {
    const drives = st.volumes.filter((v) => v.phase !== "ready").map((v) => v.label).join(", ");
    grid.setEmptyMessage([t("empty.noMatches"), t("empty.stillIndexing", { drives })]);
  } else if (state.chip !== "all") {
    grid.setEmptyMessage([t("empty.noMatches"), t("empty.filterOn", { filter: t(`chips.${state.chip}`) })]);
  } else grid.setEmptyMessage([t("empty.noMatches"), t("empty.noMatchesHint")]);
}

// ----- toolbar ---------------------------------------------------------------------------------------

const toggles: Record<string, { el: HTMLButtonElement; key: "matchCase" | "matchPath" | "wholeWord" | "matchDiacritics" }> = {
  c: { el: $("t-case"), key: "matchCase" },
  p: { el: $("t-path"), key: "matchPath" },
  w: { el: $("t-word"), key: "wholeWord" },
  a: { el: $("t-diac"), key: "matchDiacritics" },
};
const previewBtn = $<HTMLButtonElement>("t-preview");

function syncToggles(): void {
  for (const tg of Object.values(toggles)) tg.el.setAttribute("aria-pressed", String(state[tg.key]));
  previewBtn.setAttribute("aria-pressed", String(preview.visible));
}

function flip(letter: string): void {
  const tg = toggles[letter];
  if (!tg) return;
  state[tg.key] = !state[tg.key];
  syncToggles();
  void runSearch(false);
}

for (const [letter, tg] of Object.entries(toggles)) tg.el.addEventListener("click", () => flip(letter));

function togglePreview(): void {
  preview.setVisible(!preview.visible);
  syncToggles();
  if (state.settings) {
    state.settings.showPreview = preview.visible;
    persistSoon();
  }
  const r = grid.rowAt(grid.focusIndex);
  preview.show(r && !r.isDir ? r.id : null);
}
previewBtn.addEventListener("click", togglePreview);

const chipsEl = $<HTMLDivElement>("chips");
function buildChips(): void {
  chipsEl.replaceChildren();
  CHIPS.forEach((c, i) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "chip";
    b.textContent = t(`chips.${c}`);
    b.title = `${t(`chips.${c}`)} (Ctrl+${i + 1})`;
    b.setAttribute("aria-pressed", String(state.chip === c));
    b.addEventListener("click", () => setChip(c));
    chipsEl.append(b);
  });
}
function setChip(c: Chip): void {
  state.chip = c;
  buildChips();
  void runSearch(false);
}

$("help-btn").addEventListener("click", helpDialog);
$("settings-btn").addEventListener("click", () => void showSettings());

async function showSettings(): Promise<void> {
  if (!state.settings || !state.info) return;
  await openSettings({
    settings: state.settings,
    info: state.info,
    status: state.status,
    onSaved: (s) => {
      const searchChanged = s.includeHidden !== state.settings?.includeHidden || s.includeSystem !== state.settings?.includeSystem || s.resultLimit !== state.settings?.resultLimit;
      state.settings = s;
      applyTheme();
      if (searchChanged) void runSearch(true);
    },
  });
}

// ----- history -------------------------------------------------------------------------------------------

function historyOpen(): boolean {
  return !historyEl.hidden;
}

function closeHistory(focusInput = true): void {
  historyEl.hidden = true;
  historyBtn.setAttribute("aria-expanded", "false");
  if (focusInput) input.focus();
}

function openHistory(): void {
  const items = state.settings?.history ?? [];
  historyEl.replaceChildren();
  historyEl.setAttribute("aria-label", t("search.history"));
  const opts: HTMLElement[] = [];
  if (items.length === 0) {
    const p = document.createElement("div");
    p.className = "history-empty";
    p.textContent = t("search.noHistory");
    historyEl.append(p);
  }
  items.forEach((q, i) => {
    const o = document.createElement("div");
    o.className = "history-item";
    o.setAttribute("role", "option");
    o.id = `hist-${i}`;
    o.tabIndex = -1;
    o.textContent = q;
    o.addEventListener("click", () => pick(q));
    historyEl.append(o);
    opts.push(o);
  });
  if (items.length) {
    const clear = document.createElement("button");
    clear.type = "button";
    clear.className = "history-clear";
    clear.textContent = t("search.clearHistory");
    clear.addEventListener("click", async () => {
      await api.clearHistory();
      if (state.settings) state.settings.history = [];
      closeHistory();
    });
    historyEl.append(clear);
  }
  const r = searchbox.getBoundingClientRect();
  historyEl.style.left = `${r.left}px`;
  historyEl.style.top = `${r.bottom + 4}px`;
  historyEl.style.width = `${r.width}px`;
  historyEl.hidden = false;
  historyBtn.setAttribute("aria-expanded", "true");
  let pos = 0;
  opts[0]?.focus();
  historyEl.onkeydown = (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      pos = (pos + (e.key === "ArrowDown" ? 1 : opts.length - 1)) % Math.max(1, opts.length);
      opts[pos]?.focus();
    } else if (e.key === "Enter") {
      e.preventDefault();
      const q = items[pos];
      if (q !== undefined) pick(q);
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      closeHistory();
    }
  };
  function pick(q: string) {
    input.value = q;
    closeHistory();
    void runSearch(false);
  }
}

historyBtn.addEventListener("click", () => (historyOpen() ? closeHistory() : openHistory()));
window.addEventListener("mousedown", (e) => {
  if (historyOpen() && !historyEl.contains(e.target as Node) && e.target !== historyBtn && !historyBtn.contains(e.target as Node)) closeHistory(false);
});

// ----- keyboard ---------------------------------------------------------------------------------------------

function gridFocused(): boolean {
  return document.activeElement === grid.viewport;
}

document.addEventListener("keydown", (e) => {
  if (document.querySelector("dialog[open]") || menuOpen() || historyOpen()) return;
  const k = e.key;
  const ctrl = e.ctrlKey && !e.altKey;
  if (ctrl && (k === "f" || k === "F" || k === "l" || k === "L")) {
    e.preventDefault();
    input.focus();
    input.select();
    return;
  }
  if (k === "F1") {
    e.preventDefault();
    helpDialog();
    return;
  }
  if (k === "Escape") {
    e.preventDefault();
    if (input.value) {
      input.value = "";
      input.focus();
      void runSearch(false);
    } else {
      void api.hideWindow();
    }
    return;
  }
  if (e.altKey && !e.ctrlKey && !e.shiftKey) {
    const letter = k.toLowerCase();
    if (letter in toggles) {
      e.preventDefault();
      flip(letter);
      return;
    }
    if (letter === "v") {
      e.preventDefault();
      togglePreview();
      return;
    }
  }
  if (ctrl && /^[1-9]$/.test(k)) {
    e.preventDefault();
    setChip(CHIPS[Number(k) - 1]);
    return;
  }
  if (document.activeElement === input) {
    if (k === "ArrowDown" && e.altKey) {
      e.preventDefault();
      openHistory();
    } else if (k === "ArrowDown" || k === "PageDown") {
      e.preventDefault();
      grid.viewport.focus();
      void setFocus(grid.focusIndex < 0 ? 0 : grid.focusIndex, "single");
    } else if (k === "Enter") {
      e.preventDefault();
      if (grid.focusIndex < 0 && grid.total > 0) {
        void setFocus(0, "single").then(() => openSelected(e.ctrlKey ? "reveal" : e.altKey ? "properties" : "open"));
      } else {
        void openSelected(e.ctrlKey ? "reveal" : e.altKey ? "properties" : "open");
      }
    }
    return;
  }
  if (!gridFocused()) return;
  const page = grid.visibleRows();
  const mode: FocusMode = e.shiftKey ? "extend" : "single";
  const cur = grid.focusIndex;
  const move = (to: number) => {
    e.preventDefault();
    void setFocus(to, mode);
  };
  switch (k) {
    case "ArrowDown":
      return move(cur + 1);
    case "ArrowUp":
      if (cur <= 0 && !e.shiftKey) {
        e.preventDefault();
        input.focus();
        return;
      }
      return move(cur - 1);
    case "PageDown":
      return move(cur + page);
    case "PageUp":
      return move(cur - page);
    case "Home":
      return move(0);
    case "End":
      return move(grid.total - 1);
    case " ":
      if (e.ctrlKey && cur >= 0) {
        e.preventDefault();
        void setFocus(cur, "toggle");
      }
      return;
    case "Enter":
      e.preventDefault();
      void openSelected(e.ctrlKey ? "reveal" : e.altKey ? "properties" : "open");
      return;
    case "F2":
      e.preventDefault();
      void renameFocused();
      return;
    case "Delete":
      e.preventDefault();
      void deleteSelected();
      return;
    case "ContextMenu":
      e.preventDefault();
      contextAtFocus();
      return;
    case "F10":
      if (e.shiftKey) {
        e.preventDefault();
        contextAtFocus();
      }
      return;
  }
  if (ctrl && (k === "c" || k === "C")) {
    e.preventDefault();
    void copySelected(e.shiftKey ? "paths" : "files");
    return;
  }
  if (ctrl && (k === "a" || k === "A")) {
    e.preventDefault();
    void selectAll();
    return;
  }
  // Typing in the results continues the search.
  if (k.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) {
    input.focus();
  }
});

function contextAtFocus(): void {
  const rowEl = grid.viewport.querySelector<HTMLElement>(".g-row.focused");
  const r = (rowEl ?? grid.viewport).getBoundingClientRect();
  openContextMenu(r.left + 40, rowEl ? r.bottom : r.top + 40);
}

// Keep focus sensible when the window loses it mid-menu.
window.addEventListener("blur", () => closeMenu());

// ----- theme --------------------------------------------------------------------------------------------------

function applyTheme(): void {
  const root = document.documentElement;
  const theme = state.settings?.theme ?? "system";
  if (theme === "system") delete root.dataset.theme;
  else root.dataset.theme = theme;
  if (state.info?.accentColor) root.style.setProperty("--accent", state.info.accentColor);
  // Mica follows the Windows theme, so only use it when the app does too.
  root.dataset.mica = state.info?.mica && theme === "system" ? "1" : "0";
}

// ----- startup ------------------------------------------------------------------------------------------------

async function main(): Promise<void> {
  applyI18n();
  document.title = t("app.title");
  try {
    const [settings, info, status] = await Promise.all([api.getSettings(), api.appInfo(), api.getStatus()]);
    state.settings = settings;
    state.info = info;
    state.status = status;
  } catch (e) {
    showHint(t("errors.generic", { message: errorText(e) }));
  }
  const s = state.settings;
  if (s) {
    state.matchCase = s.matchCase;
    state.matchPath = s.matchPath;
    state.wholeWord = s.wholeWord;
    state.matchDiacritics = s.matchDiacritics;
    state.sort = s.sortColumn;
    state.descending = s.sortDescending;
    grid.columns = s.columns.map((c) => ({ ...c }));
    grid.setSort(state.sort, state.descending);
    preview.setVisible(s.showPreview);
  }
  applyTheme();
  syncToggles();
  buildChips();
  updateIndexStatus();

  let wasLoading = state.status?.loading ?? true;
  await on<IndexEvent>("index-status", (ev) => {
    state.status = ev.status;
    updateIndexStatus();
    if (wasLoading && !ev.status.loading) {
      wasLoading = false;
      void runSearch(true);
    } else if (ev.changed) {
      scheduleRefresh(ev.status.indexing);
    }
    updateResultsStatus();
    if (state.total === 0) updateEmpty();
  });
  await on<null>("summon", () => {
    if (document.querySelector("dialog[open]")) return;
    input.focus();
    input.select();
  });
  await on<boolean>("paused-changed", (p) => {
    if (state.status) state.status.paused = p;
    updateIndexStatus();
  });

  // Show the window first; the first search fills it a moment later.
  await api.appReady();
  input.focus();
  await runSearch(false);
  if (state.info?.hotkeyError && s?.firstRunDone) toast(state.info.hotkeyError, "error");
  if (s && !s.firstRunDone) {
    await openWelcome(async () => {
      state.settings = await api.getSettings();
      if (state.info?.hotkeyError) toast(state.info.hotkeyError, "error");
      updateIndexStatus();
      void runSearch(false);
      input.focus();
    });
  }
}

void main();
