// Typed wrappers around the Rust commands (src-tauri/src/commands.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Chip = "all" | "files" | "folders" | "documents" | "images" | "video" | "audio" | "archives" | "apps";
export type SortColumn = "relevance" | "name" | "path" | "size" | "modified" | "type";

export interface SearchArgs {
  query: string;
  chip: Chip;
  sort: SortColumn;
  descending: boolean;
  matchCase: boolean;
  matchPath: boolean;
  wholeWord: boolean;
  matchDiacritics: boolean;
  tzOffsetSecs: number;
}

export interface SearchResponse {
  searchId: number;
  total: number;
  shown: number;
  elapsedMs: number;
  limited: boolean;
  timedOut: boolean;
  cancelled: boolean;
  error: string | null;
}

export interface Row {
  id: number;
  name: string;
  hl: [number, number][];
  dir: string;
  size: number | null;
  mtime: number | null;
  isDir: boolean;
  ext: string;
  hidden: boolean;
  system: boolean;
  cloud: boolean;
  link: boolean;
  deleted: boolean;
}

export interface ColumnSetting {
  id: string;
  width: number;
}

export interface Settings {
  firstRunDone: boolean;
  hotkey: string;
  trayIcon: boolean;
  closeToTray: boolean;
  startWithWindows: boolean;
  theme: "system" | "light" | "dark";
  drives: Record<string, boolean>;
  exclusions: string[];
  includeHidden: boolean;
  includeSystem: boolean;
  fastMode: boolean;
  matchCase: boolean;
  matchPath: boolean;
  wholeWord: boolean;
  matchDiacritics: boolean;
  resultLimit: number;
  columns: ColumnSetting[];
  sortColumn: SortColumn;
  sortDescending: boolean;
  showPreview: boolean;
  history: string[];
}

export type Phase = "waiting" | "scanning" | "enumerating" | "filling" | "ready" | "error";

export interface VolumeStatus {
  label: string;
  fast: boolean;
  phase: Phase;
  count: number;
  denied: number;
  message: string | null;
}

export interface Status {
  loading: boolean;
  indexing: boolean;
  paused: boolean;
  total: number;
  volumes: VolumeStatus[];
  denied: number;
  fastMode: string;
  message: string | null;
  generation: number;
  epoch: number;
}

export interface IndexEvent {
  changed: boolean;
  status: Status;
}

export interface DriveRow {
  label: string;
  kind: "fixed" | "removable" | "network" | "other";
  fileSystem: string;
  volumeName: string;
  enabled: boolean;
  fastCapable: boolean;
}

export interface AppInfo {
  version: string;
  dataDir: string;
  logDir: string;
  portable: boolean;
  elevated: boolean;
  windowsBuild: number;
  accentColor: string | null;
  mica: boolean;
  memoryMb: number;
  hotkeyError: string | null;
}

export interface Preview {
  kind: "image" | "text" | "none";
  data: string;
  message: string | null;
}

export type OpenAction = "open" | "openWith" | "reveal" | "properties";

export const api = {
  search: (args: SearchArgs) => invoke<SearchResponse>("search", { args }),
  getRows: (searchId: number, start: number, count: number) =>
    invoke<{ stale: boolean; rows: Row[] }>("get_rows", { searchId, start, count }),
  idsInRange: (searchId: number, start: number, end: number) => invoke<number[] | null>("ids_in_range", { searchId, start, end }),
  indexOf: (searchId: number, id: number) => invoke<number | null>("index_of", { searchId, id }),
  selectionInfo: (ids: number[]) => invoke<{ files: number; folders: number; totalSize: number }>("selection_info", { ids }),
  openItems: (ids: number[], action: OpenAction) => invoke<void>("open_items", { ids, action }),
  copyItems: (ids: number[], what: "files" | "paths" | "names") => invoke<void>("copy_items", { ids, what }),
  renameItem: (id: number, newName: string) => invoke<void>("rename_item", { id, newName }),
  deleteItems: (ids: number[]) => invoke<{ deleted: number; cancelled: boolean }>("delete_items", { ids }),
  preview: (id: number) => invoke<Preview>("preview", { id }),
  typeNames: (keys: string[]) => invoke<Record<string, string>>("type_names", { keys }),
  icons: (keys: string[]) => invoke<Record<string, string>>("icons", { keys }),
  getStatus: () => invoke<Status>("get_status"),
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<{ settings: Settings; hotkeyError: string | null }>("save_settings", { settings }),
  rememberQuery: (query: string) => invoke<string[]>("remember_query", { query }),
  clearHistory: () => invoke<void>("clear_history"),
  listDrives: () => invoke<DriveRow[]>("list_drives"),
  finishFirstRun: (drives: Record<string, boolean>, fastMode: boolean) => invoke<void>("finish_first_run", { drives, fastMode }),
  enableFastMode: () => invoke<string>("enable_fast_mode"),
  disableFastMode: () => invoke<void>("disable_fast_mode"),
  rebuildIndex: () => invoke<void>("rebuild_index"),
  setPaused: (paused: boolean) => invoke<void>("set_paused", { paused }),
  appInfo: () => invoke<AppInfo>("app_info"),
  appReady: () => invoke<void>("app_ready"),
  hideWindow: () => invoke<void>("hide_window"),
  openFolder: (which: "data" | "logs") => invoke<void>("open_folder", { which }),
  openExternal: (url: string) => invoke<void>("open_external", { url }),
};

export function on<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(event, (e) => handler(e.payload));
}

/** Turns a rejected command (a string from Rust) into a message. */
export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}
