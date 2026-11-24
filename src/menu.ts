// Keyboard-accessible context menu.

export interface MenuEntry {
  id: string;
  label: string;
  shortcut?: string;
  separatorBefore?: boolean;
  disabled?: boolean;
}

let cleanup: (() => void) | null = null;

export function menuOpen(): boolean {
  return cleanup !== null;
}

export function closeMenu(): void {
  cleanup?.();
}

/** Shows the menu at (x, y). Focus returns to `restore` when it closes. */
export function showMenu(x: number, y: number, label: string, entries: MenuEntry[], onPick: (id: string) => void, restore: HTMLElement): void {
  closeMenu();
  const menu = document.getElementById("menu")!;
  menu.replaceChildren();
  menu.setAttribute("aria-label", label);
  const items: HTMLButtonElement[] = [];
  for (const e of entries) {
    if (e.separatorBefore) {
      const sep = document.createElement("div");
      sep.className = "menu-sep";
      sep.setAttribute("role", "separator");
      menu.append(sep);
    }
    const b = document.createElement("button");
    b.type = "button";
    b.className = "menu-item";
    b.setAttribute("role", "menuitem");
    b.tabIndex = -1;
    b.disabled = !!e.disabled;
    const l = document.createElement("span");
    l.textContent = e.label;
    b.append(l);
    if (e.shortcut) {
      const k = document.createElement("kbd");
      k.textContent = e.shortcut;
      b.append(k);
    }
    b.addEventListener("click", () => {
      closeMenu();
      onPick(e.id);
    });
    menu.append(b);
    items.push(b);
  }
  menu.hidden = false;
  const r = menu.getBoundingClientRect();
  const left = Math.min(x, window.innerWidth - r.width - 4);
  const top = y + r.height > window.innerHeight - 4 ? Math.max(4, y - r.height) : y;
  menu.style.left = `${Math.max(4, left)}px`;
  menu.style.top = `${top}px`;
  const enabled = items.filter((i) => !i.disabled);
  let pos = 0;
  enabled[0]?.focus();
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape" || ev.key === "Tab") {
      ev.preventDefault();
      closeMenu();
    } else if (ev.key === "ArrowDown" || ev.key === "ArrowUp" || ev.key === "Home" || ev.key === "End") {
      ev.preventDefault();
      if (ev.key === "Home") pos = 0;
      else if (ev.key === "End") pos = enabled.length - 1;
      else pos = (pos + (ev.key === "ArrowDown" ? 1 : enabled.length - 1)) % enabled.length;
      enabled[pos]?.focus();
    }
    ev.stopPropagation();
  };
  const onDown = (ev: MouseEvent) => {
    if (!menu.contains(ev.target as Node)) closeMenu();
  };
  menu.addEventListener("keydown", onKey);
  window.addEventListener("mousedown", onDown, true);
  window.addEventListener("blur", closeMenu);
  cleanup = () => {
    cleanup = null;
    menu.hidden = true;
    menu.removeEventListener("keydown", onKey);
    window.removeEventListener("mousedown", onDown, true);
    window.removeEventListener("blur", closeMenu);
    restore.focus();
  };
}
