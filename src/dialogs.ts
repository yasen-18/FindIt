// Modal dialogs built on <dialog>, which gives focus trapping, Esc to close and
// screen reader semantics for free.
import { t, tRows } from "./i18n";

let dialogSeq = 0;

export interface DialogParts {
  dlg: HTMLDialogElement;
  body: HTMLDivElement;
  footer: HTMLDivElement;
  close(): void;
}

export function makeDialog(title: string, cls = ""): DialogParts {
  const dlg = document.createElement("dialog");
  dlg.className = `dlg ${cls}`.trim();
  const id = `dlg-title-${++dialogSeq}`;
  dlg.setAttribute("aria-labelledby", id);
  const h = document.createElement("h2");
  h.id = id;
  h.textContent = title;
  const body = document.createElement("div");
  body.className = "dlg-body";
  const footer = document.createElement("div");
  footer.className = "dlg-footer";
  dlg.append(h, body, footer);
  document.body.append(dlg);
  dlg.addEventListener("close", () => dlg.remove());
  return { dlg, body, footer, close: () => dlg.close() };
}

export function button(label: string, kind: "primary" | "secondary" | "danger" = "secondary"): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = `btn ${kind}`;
  b.textContent = label;
  return b;
}

export function para(text: string, cls = ""): HTMLParagraphElement {
  const p = document.createElement("p");
  if (cls) p.className = cls;
  p.textContent = text;
  return p;
}

/** Yes/no question. Resolves true when confirmed. */
export function confirmDialog(opts: { title: string; message: string; note?: string; confirm: string; danger?: boolean }): Promise<boolean> {
  return new Promise((resolve) => {
    const d = makeDialog(opts.title);
    d.body.append(para(opts.message));
    if (opts.note) d.body.append(para(opts.note, "muted"));
    const ok = button(opts.confirm, opts.danger ? "danger" : "primary");
    const cancel = button(t("dialogs.cancel"));
    d.footer.append(cancel, ok);
    let result = false;
    ok.addEventListener("click", () => {
      result = true;
      d.close();
    });
    cancel.addEventListener("click", () => d.close());
    d.dlg.addEventListener("close", () => resolve(result));
    d.dlg.showModal();
    ok.focus();
  });
}

/** Text input. `submit` returns an error message to keep the dialog open. */
export function promptDialog(opts: {
  title: string;
  label: string;
  value: string;
  confirm: string;
  select?: [number, number];
  submit(value: string): Promise<string | null>;
}): Promise<boolean> {
  return new Promise((resolve) => {
    const d = makeDialog(opts.title);
    const label = document.createElement("label");
    label.className = "field";
    const span = document.createElement("span");
    span.textContent = opts.label;
    const input = document.createElement("input");
    input.type = "text";
    input.value = opts.value;
    input.spellcheck = false;
    const err = para("", "error");
    err.setAttribute("role", "alert");
    err.hidden = true;
    label.append(span, input);
    d.body.append(label, err);
    const ok = button(opts.confirm, "primary");
    const cancel = button(t("dialogs.cancel"));
    d.footer.append(cancel, ok);
    let done = false;
    const go = async () => {
      ok.disabled = true;
      const problem = await opts.submit(input.value);
      ok.disabled = false;
      if (problem) {
        err.textContent = problem;
        err.hidden = false;
        input.setAttribute("aria-invalid", "true");
        input.focus();
        return;
      }
      done = true;
      d.close();
    };
    ok.addEventListener("click", go);
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void go();
      }
    });
    cancel.addEventListener("click", () => d.close());
    d.dlg.addEventListener("close", () => resolve(done));
    d.dlg.showModal();
    input.focus();
    const [s, e] = opts.select ?? [0, input.value.length];
    input.setSelectionRange(s, e);
  });
}

/** The honest disclosure shown before asking for administrator rights (spec 8). */
export function fastModeDialog(): Promise<boolean> {
  return new Promise((resolve) => {
    const d = makeDialog(t("dialogs.fast.title"), "wide");
    for (const k of ["what", "admin", "helper"]) d.body.append(para(t(`dialogs.fast.${k}`)));
    d.body.append(para(t("dialogs.fast.visibility"), "callout"));
    d.body.append(para(t("dialogs.fast.decline"), "muted"));
    const ok = button(t("dialogs.fast.confirm"), "primary");
    const no = button(t("dialogs.fast.notNow"));
    d.footer.append(no, ok);
    let result = false;
    ok.addEventListener("click", () => {
      result = true;
      d.close();
    });
    no.addEventListener("click", () => d.close());
    d.dlg.addEventListener("close", () => resolve(result));
    d.dlg.showModal();
    no.focus();
  });
}

function table(rows: string[][], caption: string): HTMLTableElement {
  const tbl = document.createElement("table");
  tbl.className = "syntax";
  const cap = document.createElement("caption");
  cap.textContent = caption;
  tbl.append(cap);
  for (const [a, b] of rows) {
    const tr = document.createElement("tr");
    const th = document.createElement("th");
    th.scope = "row";
    const code = document.createElement("code");
    code.textContent = a;
    th.append(code);
    const td = document.createElement("td");
    td.textContent = b;
    tr.append(th, td);
    tbl.append(tr);
  }
  return tbl;
}

export function helpDialog(): void {
  const d = makeDialog(t("help.title"), "wide");
  d.body.append(para(t("help.intro")));
  d.body.append(table(tRows("help.rows"), t("help.title")));
  d.body.append(table(tRows("help.keys"), t("help.keysTitle")));
  const close = button(t("dialogs.close"), "primary");
  close.addEventListener("click", () => d.close());
  d.footer.append(close);
  d.dlg.showModal();
  close.focus();
}

let toastTimer: number | undefined;

/** Short non-blocking message at the bottom of the window. */
export function toast(message: string, kind: "info" | "error" = "info"): void {
  const el = document.getElementById("toast");
  if (!el) return;
  el.textContent = message;
  el.className = `toast ${kind}`;
  el.setAttribute("role", kind === "error" ? "alert" : "status");
  el.hidden = false;
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (el.hidden = true), kind === "error" ? 6000 : 2500);
}
