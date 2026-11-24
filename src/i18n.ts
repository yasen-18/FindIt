// Every user-visible string lives in locales/*.json so the UI can be translated.
import en from "./locales/en.json";

type Entry = string | string[][] | Dict;
interface Dict {
  [key: string]: Entry;
}

const dict = en as unknown as Dict;

function lookup(key: string): Entry | undefined {
  let cur: Entry | undefined = dict;
  for (const part of key.split(".")) {
    if (cur === undefined || typeof cur === "string" || Array.isArray(cur)) return undefined;
    cur = cur[part];
  }
  return cur;
}

/** Translates `key`, replacing `{name}` placeholders with `params`. */
export function t(key: string, params?: Record<string, string | number>): string {
  const v = lookup(key);
  let s = typeof v === "string" ? v : key;
  if (params) {
    for (const [k, val] of Object.entries(params)) s = s.split(`{${k}}`).join(String(val));
  }
  return s;
}

/** Table-shaped resources such as the syntax help. */
export function tRows(key: string): string[][] {
  const v = lookup(key);
  return Array.isArray(v) ? v : [];
}

/** Fills `data-i18n*` attributes below `root`. */
export function applyI18n(root: ParentNode = document): void {
  root.querySelectorAll<HTMLElement>("[data-i18n]").forEach((el) => {
    el.textContent = t(el.dataset.i18n!);
  });
  root.querySelectorAll<HTMLElement>("[data-i18n-title]").forEach((el) => {
    const text = t(el.dataset.i18nTitle!);
    el.title = text;
    if (!el.getAttribute("aria-label")) el.setAttribute("aria-label", text.replace(/\s*\(.*\)$/, ""));
  });
  root.querySelectorAll<HTMLElement>("[data-i18n-aria]").forEach((el) => {
    el.setAttribute("aria-label", t(el.dataset.i18nAria!));
  });
  root.querySelectorAll<HTMLInputElement>("[data-i18n-placeholder]").forEach((el) => {
    el.placeholder = t(el.dataset.i18nPlaceholder!);
  });
}
