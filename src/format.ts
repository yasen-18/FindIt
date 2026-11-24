// Number, size and date formatting in the user's locale.

const intFmt = new Intl.NumberFormat();
const decFmt = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
const dateFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "short" });

export function formatCount(n: number): string {
  return intFmt.format(n);
}

export function formatSize(bytes: number | null): string {
  if (bytes === null) return "";
  if (bytes < 1024) return `${intFmt.format(bytes)} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 ? intFmt.format(Math.round(v)) : decFmt.format(v)} ${units[i]}`;
}

export function formatDate(unixSecs: number | null): string {
  if (unixSecs === null || unixSecs <= 0) return "";
  return dateFmt.format(new Date(unixSecs * 1000));
}

/** Offset of local time from UTC in seconds (local = utc + offset). */
export function tzOffsetSecs(): number {
  return -new Date().getTimezoneOffset() * 60;
}

/** Compares two dotted versions ("1.2.10" > "1.2.9"). */
export function newerVersion(latest: string, current: string): boolean {
  const parse = (v: string) => v.replace(/^v/i, "").split(/[.+-]/).slice(0, 3).map((x) => parseInt(x, 10) || 0);
  const a = parse(latest);
  const b = parse(current);
  for (let i = 0; i < 3; i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) > (b[i] ?? 0);
  }
  return false;
}
