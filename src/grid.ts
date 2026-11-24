// Virtualized results grid. Only the visible rows exist in the DOM; their data
// is fetched from Rust in blocks, so a million results scroll as smoothly as ten.
import type { ColumnSetting, Row, SortColumn } from "./api";
import { formatDate, formatSize } from "./format";
import { t } from "./i18n";

const BLOCK = 100;
const MAX_CACHE = 6000;
// Browsers cap element heights (Chromium at about 33 million px); beyond this
// the scrollbar is scaled.
const MAX_SCROLL_PX = 8_000_000;

export interface GridCallbacks {
  fetchRows(searchId: number, start: number, count: number): Promise<Row[] | null>;
  onRowPointer(index: number, row: Row | undefined, e: MouseEvent): void;
  onRowActivate(index: number, row: Row, e: MouseEvent): void;
  onContextMenu(index: number, row: Row | undefined, e: MouseEvent): void;
  onSort(col: SortColumn): void;
  onColumnsChanged(cols: ColumnSetting[]): void;
  isSelected(id: number): boolean;
  typeName(row: Row): string;
  iconUrl(row: Row): string | null;
  onStale(): void;
  onRendered(rows: Row[]): void;
}

const COLUMN_SORT: Record<string, SortColumn> = { name: "name", path: "path", size: "size", modified: "modified", type: "type" };

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  return e;
}

/** Name with the matched parts in bold. Built with text nodes: names are untrusted. */
export function highlighted(name: string, hl: [number, number][]): DocumentFragment {
  const frag = document.createDocumentFragment();
  let pos = 0;
  for (const [s, e] of hl) {
    if (s < pos || e > name.length || s >= e) continue;
    if (s > pos) frag.append(name.slice(pos, s));
    const b = el("b");
    b.textContent = name.slice(s, e);
    frag.append(b);
    pos = e;
  }
  if (pos < name.length) frag.append(name.slice(pos));
  return frag;
}

export class Grid {
  readonly viewport: HTMLDivElement;
  private head: HTMLDivElement;
  private headRow: HTMLDivElement;
  private body: HTMLDivElement;
  private empty: HTMLDivElement;
  private pool: HTMLDivElement[] = [];
  private cache = new Map<number, Row>();
  private pending = new Set<number>();
  private rafPending = false;
  private headHeight = 30;
  readonly rowHeight = 28;

  searchId = 0;
  total = 0;
  focusIndex = -1;
  columns: ColumnSetting[];
  sort: SortColumn = "relevance";
  descending = false;

  constructor(
    host: HTMLElement,
    columns: ColumnSetting[],
    private cb: GridCallbacks,
  ) {
    this.columns = columns;
    this.viewport = el("div", "g-viewport");
    this.viewport.setAttribute("role", "grid");
    this.viewport.setAttribute("aria-multiselectable", "true");
    this.viewport.setAttribute("aria-label", t("grid.label"));
    this.viewport.tabIndex = 0;
    this.head = el("div", "g-head");
    this.head.setAttribute("role", "rowgroup");
    this.headRow = el("div", "g-row g-headrow");
    this.headRow.setAttribute("role", "row");
    this.headRow.setAttribute("aria-rowindex", "1");
    this.head.append(this.headRow);
    this.body = el("div", "g-body");
    this.body.setAttribute("role", "rowgroup");
    this.viewport.append(this.head, this.body);
    this.empty = el("div", "g-empty");
    this.empty.hidden = true;
    host.append(this.viewport, this.empty);

    this.viewport.addEventListener("scroll", () => this.schedule(), { passive: true });
    new ResizeObserver(() => this.schedule()).observe(this.viewport);
    this.body.addEventListener("mousedown", (e) => this.rowEvent(e, (i, r) => this.cb.onRowPointer(i, r, e)));
    this.body.addEventListener("dblclick", (e) =>
      this.rowEvent(e, (i, r) => {
        if (r) this.cb.onRowActivate(i, r, e);
      }),
    );
    this.body.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      this.rowEvent(e, (i, r) => this.cb.onContextMenu(i, r, e));
    });
    this.buildHeader();
  }

  private rowEvent(e: MouseEvent, f: (index: number, row: Row | undefined) => void): void {
    const rowEl = (e.target as HTMLElement).closest<HTMLElement>(".g-row");
    if (!rowEl || rowEl.dataset.index === undefined) return;
    const index = Number(rowEl.dataset.index);
    if (index >= 0 && index < this.total) f(index, this.cache.get(index));
  }

  // ----- columns ---------------------------------------------------------------------

  private applyWidths(): void {
    const tpl = this.columns.map((c) => `${c.width}px`).join(" ");
    this.viewport.style.setProperty("--cols", tpl);
    const total = this.columns.reduce((a, c) => a + c.width, 0);
    this.viewport.style.setProperty("--rows-width", `${total}px`);
  }

  buildHeader(): void {
    this.applyWidths();
    this.headRow.replaceChildren();
    this.columns.forEach((col, ci) => {
      const cell = el("div", `g-cell g-hcell col-${col.id}`);
      cell.setAttribute("role", "columnheader");
      cell.setAttribute("aria-colindex", String(ci + 1));
      const sortKey = COLUMN_SORT[col.id];
      const active = this.sort === sortKey;
      cell.setAttribute("aria-sort", active ? (this.descending ? "descending" : "ascending") : "none");
      const btn = el("button", "g-sortbtn");
      btn.type = "button";
      btn.textContent = t(`columns.${col.id}`);
      if (active) {
        const arrow = el("span", "g-arrow");
        arrow.setAttribute("aria-hidden", "true");
        arrow.textContent = this.descending ? "▾" : "▴";
        btn.append(arrow);
      }
      btn.addEventListener("click", () => this.cb.onSort(sortKey));
      cell.append(btn);
      cell.draggable = true;
      cell.addEventListener("dragstart", (e) => {
        e.dataTransfer?.setData("text/x-findit-col", col.id);
        cell.classList.add("dragging");
      });
      cell.addEventListener("dragend", () => cell.classList.remove("dragging"));
      cell.addEventListener("dragover", (e) => {
        if (e.dataTransfer?.types.includes("text/x-findit-col")) e.preventDefault();
      });
      cell.addEventListener("drop", (e) => {
        e.preventDefault();
        const from = e.dataTransfer?.getData("text/x-findit-col");
        if (!from || from === col.id) return;
        const moving = this.columns.find((c) => c.id === from);
        if (!moving) return;
        const rest = this.columns.filter((c) => c.id !== from);
        const at = rest.findIndex((c) => c.id === col.id);
        rest.splice(at, 0, moving);
        this.columns = rest;
        this.buildHeader();
        this.refreshRows(true);
        this.cb.onColumnsChanged(this.columns);
      });
      const handle = el("span", "g-resize");
      handle.setAttribute("aria-hidden", "true");
      handle.title = t("columns.resize");
      handle.addEventListener("pointerdown", (e) => this.startResize(e, col, handle));
      handle.addEventListener("dblclick", (e) => e.stopPropagation());
      cell.append(handle);
      this.headRow.append(cell);
    });
    this.viewport.setAttribute("aria-colcount", String(this.columns.length));
  }

  private startResize(e: PointerEvent, col: ColumnSetting, handle: HTMLElement): void {
    e.preventDefault();
    e.stopPropagation();
    const startX = e.clientX;
    const startW = col.width;
    handle.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      col.width = Math.max(48, Math.min(2000, Math.round(startW + ev.clientX - startX)));
      this.applyWidths();
    };
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", up);
      this.cb.onColumnsChanged(this.columns);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", up);
  }

  setSort(sort: SortColumn, descending: boolean): void {
    this.sort = sort;
    this.descending = descending;
    this.buildHeader();
  }

  // ----- data ----------------------------------------------------------------------------

  /** Shows a new result set. `keepScroll` keeps the position (live refresh). */
  setResult(searchId: number, total: number, keepScroll: boolean): void {
    this.searchId = searchId;
    this.total = total;
    this.cache.clear();
    this.pending.clear();
    this.viewport.setAttribute("aria-rowcount", String(total + 1));
    this.body.style.height = `${this.virtualHeight()}px`;
    if (!keepScroll) {
      this.viewport.scrollTop = 0;
      this.focusIndex = -1;
    }
    if (this.focusIndex >= total) this.focusIndex = total - 1;
    this.render();
  }

  setEmptyMessage(lines: string[] | null): void {
    this.empty.hidden = !lines;
    this.empty.replaceChildren();
    for (const [i, line] of (lines ?? []).entries()) {
      const p = el("p", i === 0 ? "g-empty-title" : "g-empty-text");
      p.textContent = line;
      this.empty.append(p);
    }
  }

  rowAt(index: number): Row | undefined {
    return this.cache.get(index);
  }

  /** Loads (if needed) and returns the row at `index`. */
  async ensureRow(index: number): Promise<Row | undefined> {
    if (index < 0 || index >= this.total) return undefined;
    const hit = this.cache.get(index);
    if (hit) return hit;
    await this.fetchBlock(Math.floor(index / BLOCK));
    return this.cache.get(index);
  }

  private async fetchBlock(block: number): Promise<void> {
    if (this.pending.has(block)) {
      while (this.pending.has(block)) await new Promise((r) => setTimeout(r, 10));
      return;
    }
    this.pending.add(block);
    const searchId = this.searchId;
    try {
      const rows = await this.cb.fetchRows(searchId, block * BLOCK, BLOCK);
      if (searchId !== this.searchId) return;
      if (rows === null) {
        this.cb.onStale();
        return;
      }
      if (this.cache.size > MAX_CACHE) this.cache.clear();
      rows.forEach((r, i) => this.cache.set(block * BLOCK + i, r));
      this.schedule();
    } finally {
      this.pending.delete(block);
    }
  }

  // ----- geometry ------------------------------------------------------------------------

  private virtualHeight(): number {
    return Math.min(this.total * this.rowHeight, MAX_SCROLL_PX);
  }

  private visibleArea(): number {
    return Math.max(this.rowHeight, this.viewport.clientHeight - this.headHeight);
  }

  visibleRows(): number {
    return Math.max(1, Math.floor(this.visibleArea() / this.rowHeight));
  }

  private scaled(): boolean {
    return this.total * this.rowHeight > MAX_SCROLL_PX;
  }

  private scrollMax(): number {
    return Math.max(1, this.viewport.scrollHeight - this.viewport.clientHeight);
  }

  private maxFirst(): number {
    return Math.max(0, this.total - this.visibleArea() / this.rowHeight);
  }

  /** Fractional index of the first visible row. */
  private firstExact(): number {
    const st = this.viewport.scrollTop;
    if (!this.scaled()) return st / this.rowHeight;
    return (st / this.scrollMax()) * this.maxFirst();
  }

  private scrollToFirst(first: number): void {
    const f = Math.max(0, Math.min(first, this.maxFirst()));
    this.viewport.scrollTop = this.scaled() ? (f / Math.max(1, this.maxFirst())) * this.scrollMax() : f * this.rowHeight;
  }

  /** Scrolls the minimum needed to show `index`. */
  scrollIntoView(index: number): void {
    const first = this.firstExact();
    const rows = this.visibleArea() / this.rowHeight;
    if (index < first) this.scrollToFirst(index);
    else if (index + 1 > first + rows) this.scrollToFirst(index + 1 - rows);
    this.render();
  }

  firstVisibleIndex(): number {
    return Math.floor(this.firstExact());
  }

  // ----- rendering ------------------------------------------------------------------------

  schedule(): void {
    if (this.rafPending) return;
    this.rafPending = true;
    requestAnimationFrame(() => {
      this.rafPending = false;
      this.render();
    });
  }

  /** Re-renders the visible rows (selection or focus changed). */
  refreshRows(force = false): void {
    if (force) this.pool.forEach((r) => delete r.dataset.rowId);
    this.render();
  }

  private render(): void {
    this.headHeight = this.head.offsetHeight || this.headHeight;
    const first = this.firstExact();
    const start = Math.max(0, Math.floor(first));
    const count = this.total === 0 ? 0 : Math.min(this.total - start, Math.ceil(this.visibleArea() / this.rowHeight) + 1);
    while (this.pool.length < count) {
      const r = el("div", "g-row");
      r.setAttribute("role", "row");
      this.body.append(r);
      this.pool.push(r);
    }
    const st = this.viewport.scrollTop;
    const shown: Row[] = [];
    const missing = new Set<number>();
    for (let slot = 0; slot < this.pool.length; slot++) {
      const rowEl = this.pool[slot];
      if (slot >= count) {
        rowEl.hidden = true;
        continue;
      }
      const index = start + slot;
      rowEl.hidden = false;
      rowEl.style.transform = `translateY(${Math.round(st + (index - first) * this.rowHeight)}px)`;
      rowEl.id = `row-${slot}`;
      rowEl.setAttribute("aria-rowindex", String(index + 2));
      const row = this.cache.get(index);
      if (row) {
        shown.push(row);
        this.fill(rowEl, row, index);
      } else {
        missing.add(Math.floor(index / BLOCK));
        if (rowEl.dataset.index !== String(index)) this.placeholder(rowEl, index);
      }
      const selected = row ? this.cb.isSelected(row.id) : false;
      rowEl.classList.toggle("selected", selected);
      rowEl.setAttribute("aria-selected", String(selected));
      rowEl.classList.toggle("focused", index === this.focusIndex);
      if (index === this.focusIndex) this.viewport.setAttribute("aria-activedescendant", rowEl.id);
    }
    if (this.focusIndex < start || this.focusIndex >= start + count) this.viewport.removeAttribute("aria-activedescendant");
    missing.forEach((b) => void this.fetchBlock(b));
    if (shown.length) this.cb.onRendered(shown);
  }

  private placeholder(rowEl: HTMLDivElement, index: number): void {
    rowEl.dataset.index = String(index);
    delete rowEl.dataset.rowId;
    rowEl.className = "g-row loading";
    rowEl.replaceChildren();
    const cell = el("div", "g-cell");
    cell.setAttribute("role", "gridcell");
    cell.textContent = "";
    rowEl.append(cell);
  }

  private fill(rowEl: HTMLDivElement, row: Row, index: number): void {
    const key = `${row.id}:${row.name}:${row.size}:${row.mtime}:${row.deleted}`;
    const typeName = this.cb.typeName(row);
    const icon = this.cb.iconUrl(row);
    const stamp = `${key}|${typeName}|${icon ? 1 : 0}|${this.columns.map((c) => c.id).join(",")}`;
    rowEl.dataset.index = String(index);
    if (rowEl.dataset.rowId === stamp) return;
    rowEl.dataset.rowId = stamp;
    rowEl.className = "g-row";
    rowEl.classList.toggle("dim", row.hidden || row.system);
    rowEl.classList.toggle("deleted", row.deleted);
    const fullPath = row.dir ? (row.dir.endsWith("\\") ? row.dir + row.name : `${row.dir}\\${row.name}`) : row.name;
    const cells: HTMLElement[] = [];
    for (const col of this.columns) {
      const c = el("div", `g-cell col-${col.id}`);
      c.setAttribute("role", "gridcell");
      switch (col.id) {
        case "name": {
          const img = el("span", "g-icon");
          img.setAttribute("aria-hidden", "true");
          if (icon) img.style.backgroundImage = `url("${icon}")`;
          else img.classList.add(row.isDir ? "fallback-dir" : "fallback-file");
          const name = el("span", "g-name");
          name.append(highlighted(row.name, row.hl));
          c.append(img, name);
          if (row.cloud || row.link) {
            const badge = el("span", "g-badge");
            badge.textContent = row.cloud ? "☁" : "↪";
            badge.title = t(row.cloud ? "grid.cloud" : "grid.link");
            badge.setAttribute("aria-label", badge.title);
            c.append(badge);
          }
          c.title = row.deleted ? `${fullPath} (${t("grid.deleted")})` : fullPath;
          break;
        }
        case "path":
          c.textContent = row.dir;
          c.title = row.dir;
          break;
        case "size":
          c.textContent = row.isDir ? "" : formatSize(row.size);
          break;
        case "modified":
          c.textContent = formatDate(row.mtime);
          break;
        case "type":
          c.textContent = typeName;
          break;
      }
      cells.push(c);
    }
    rowEl.replaceChildren(...cells);
  }
}
