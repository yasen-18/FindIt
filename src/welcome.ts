// First-run welcome in three steps: pick drives, choose the mode, start indexing.
import { api, errorText, type DriveRow } from "./api";
import { button, makeDialog, para, toast } from "./dialogs";
import { t } from "./i18n";

export async function openWelcome(onDone: () => void): Promise<void> {
  let drives: DriveRow[] = [];
  try {
    drives = await api.listDrives();
  } catch (e) {
    toast(errorText(e), "error");
  }
  const chosen = new Map(drives.map((d) => [d.label, d.enabled]));
  let fast = false;
  let step = 1;

  const d = makeDialog(t("welcome.title"), "wide welcome");
  // The wizard must finish: indexing only starts at the end.
  d.dlg.addEventListener("cancel", (e) => e.preventDefault());
  const stepLabel = para("", "muted step");
  const content = document.createElement("div");
  d.body.append(stepLabel, content);
  const back = button(t("welcome.back"));
  const next = button(t("welcome.next"), "primary");
  d.footer.append(back, next);

  const fastPossible = () => drives.some((x) => x.fastCapable && chosen.get(x.label));

  const render = () => {
    stepLabel.textContent = t("welcome.step", { n: step });
    back.hidden = step === 1;
    next.textContent = step === 3 ? t("welcome.start") : t("welcome.next");
    content.replaceChildren();
    const h = document.createElement("h3");
    content.append(h);
    if (step === 1) {
      h.textContent = t("welcome.drivesTitle");
      content.append(para(t("welcome.drivesBody")));
      const list = document.createElement("div");
      list.className = "drives";
      for (const dr of drives) {
        const row = document.createElement("label");
        row.className = "check";
        const input = document.createElement("input");
        input.type = "checkbox";
        input.checked = !!chosen.get(dr.label);
        input.addEventListener("change", () => {
          chosen.set(dr.label, input.checked);
          next.disabled = ![...chosen.values()].some(Boolean);
        });
        const span = document.createElement("span");
        const kind = t(`settings.indexing.kind${dr.kind[0].toUpperCase()}${dr.kind.slice(1)}`);
        span.textContent = `${dr.label}  ${kind}${dr.volumeName ? ` (${dr.volumeName})` : ""}`;
        row.append(input, span);
        list.append(row);
      }
      content.append(list);
      next.disabled = ![...chosen.values()].some(Boolean);
    } else if (step === 2) {
      h.textContent = t("welcome.fastTitle");
      next.disabled = false;
      if (!fastPossible()) {
        fast = false;
        content.append(para(t("welcome.fastNotAvailable")));
      } else {
        content.append(para(t("welcome.fastBody")));
        const group = document.createElement("div");
        group.setAttribute("role", "radiogroup");
        group.className = "choices";
        const details = document.createElement("div");
        details.className = "disclosure";
        for (const k of ["what", "helper"]) details.append(para(t(`dialogs.fast.${k}`), "muted"));
        details.append(para(t("dialogs.fast.visibility"), "callout"));
        const option = (value: boolean, label: string) => {
          const row = document.createElement("label");
          row.className = "check";
          const input = document.createElement("input");
          input.type = "radio";
          input.name = "mode";
          input.checked = fast === value;
          input.addEventListener("change", () => {
            fast = value;
            details.hidden = !fast;
          });
          const span = document.createElement("span");
          span.textContent = label;
          row.append(input, span);
          return row;
        };
        group.append(option(false, t("welcome.standardOption")), option(true, t("welcome.fastOption")));
        details.hidden = !fast;
        content.append(group, details);
      }
    } else {
      h.textContent = t("welcome.startTitle");
      content.append(para(t("welcome.startBody")));
      next.disabled = false;
    }
    const firstInput = content.querySelector<HTMLInputElement>("input");
    if (firstInput) firstInput.focus();
    else next.focus();
  };

  back.addEventListener("click", () => {
    step = Math.max(1, step - 1);
    render();
  });
  next.addEventListener("click", async () => {
    if (step < 3) {
      step++;
      render();
      return;
    }
    next.disabled = true;
    try {
      await api.finishFirstRun(Object.fromEntries(chosen), fast && fastPossible());
      d.close();
      onDone();
    } catch (e) {
      toast(errorText(e), "error");
      next.disabled = false;
    }
  });
  d.dlg.showModal();
  render();
}
