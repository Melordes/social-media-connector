// Date and time formatting in the user's locale.

const locale = navigator.language || "en";

const timeFmt = new Intl.DateTimeFormat(locale, { hour: "numeric", minute: "2-digit" });
const dayFmt = new Intl.DateTimeFormat(locale, { weekday: "short", day: "numeric", month: "short" });
const dateTimeFmt = new Intl.DateTimeFormat(locale, {
  weekday: "short",
  day: "numeric",
  month: "short",
  year: "numeric",
  hour: "numeric",
  minute: "2-digit",
});
const monthFmt = new Intl.DateTimeFormat(locale, { month: "long", year: "numeric" });
const shortDateFmt = new Intl.DateTimeFormat(locale, { day: "numeric", month: "short" });
const weekdayFmt = new Intl.DateTimeFormat(locale, { weekday: "short" });

export const fmt = {
  time: (d: Date | number) => timeFmt.format(d),
  day: (d: Date | number) => dayFmt.format(d),
  dateTime: (d: Date | number) => dateTimeFmt.format(d),
  month: (d: Date | number) => monthFmt.format(d),
  shortDate: (d: Date | number) => shortDateFmt.format(d),
  weekday: (d: Date | number) => weekdayFmt.format(d),
};

/** "3m", "5h", "2d" or a date for older items. */
export function relativeTime(iso: string | number, now = Date.now()): string {
  const t = typeof iso === "number" ? iso : Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 45) return "now";
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) return `${Math.round(s / 3600)}h`;
  if (s < 7 * 86400) return `${Math.round(s / 86400)}d`;
  return shortDateFmt.format(t);
}

/** "in 3 hours", "in 2 days", "5 minutes ago". */
export function relativeFuture(ms: number, now = Date.now()): string {
  const diff = ms - now;
  const abs = Math.abs(diff);
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  if (abs < 3600_000) return rtf.format(Math.round(diff / 60_000), "minute");
  if (abs < 86400_000) return rtf.format(Math.round(diff / 3600_000), "hour");
  return rtf.format(Math.round(diff / 86400_000), "day");
}

export function startOfDay(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}

export function addDays(d: Date, n: number): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n, d.getHours(), d.getMinutes());
}

export function sameDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

/** The 42 days (6 weeks) shown in a month grid. */
export function monthGrid(month: Date, weekStartsMonday: boolean): Date[] {
  const first = new Date(month.getFullYear(), month.getMonth(), 1);
  const offset = (first.getDay() - (weekStartsMonday ? 1 : 0) + 7) % 7;
  const start = addDays(first, -offset);
  return Array.from({ length: 42 }, (_, i) => addDays(start, i));
}

export function dayKey(d: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** Next full hour from now, at least 15 minutes ahead. */
export function nextSlot(now = new Date()): Date {
  const d = new Date(now.getFullYear(), now.getMonth(), now.getDate(), now.getHours() + 1, 0);
  if (d.getTime() - now.getTime() < 15 * 60_000) d.setHours(d.getHours() + 1);
  return d;
}

export function toDateInput(d: Date): string {
  return dayKey(d);
}

export function toTimeInput(d: Date): string {
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

export function fromInputs(date: string, time: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(date);
  const t = /^(\d{1,2}):(\d{2})$/.exec(time);
  if (!m || !t) return null;
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]), Number(t[1]), Number(t[2]));
  return Number.isNaN(d.getTime()) ? null : d;
}
