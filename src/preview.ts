// Optional preview pane for images and text.
import { api, errorText } from "./api";
import { t } from "./i18n";

export class PreviewPane {
  private timer: number | undefined;
  private current = -1;
  private token = 0;

  constructor(private el: HTMLElement) {
    this.el.setAttribute("role", "region");
    this.message(t("preview.empty"));
  }

  get visible(): boolean {
    return !this.el.hidden;
  }

  setVisible(v: boolean): void {
    this.el.hidden = !v;
    if (!v) this.current = -1;
  }

  /** Shows `id` after a short pause, so arrowing through results stays fast. */
  show(id: number | null): void {
    if (!this.visible) return;
    window.clearTimeout(this.timer);
    if (id === null) {
      this.current = -1;
      this.message(t("preview.empty"));
      return;
    }
    if (id === this.current) return;
    this.timer = window.setTimeout(() => void this.load(id), 120);
  }

  private message(text: string): void {
    const p = document.createElement("p");
    p.className = "pv-message";
    p.textContent = text;
    this.el.replaceChildren(p);
  }

  private async load(id: number): Promise<void> {
    this.current = id;
    const token = ++this.token;
    try {
      const p = await api.preview(id);
      if (token !== this.token) return;
      if (p.kind === "image") {
        const img = document.createElement("img");
        img.className = "pv-image";
        img.alt = "";
        img.src = p.data;
        this.el.replaceChildren(img);
      } else if (p.kind === "text") {
        const pre = document.createElement("pre");
        pre.className = "pv-text";
        pre.tabIndex = 0;
        pre.textContent = p.data;
        this.el.replaceChildren(pre);
      } else {
        this.message(p.message ?? t("preview.unsupported"));
      }
    } catch (e) {
      if (token === this.token) this.message(errorText(e));
    }
  }
}
