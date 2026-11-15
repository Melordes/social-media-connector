// Character counting, mirroring src-tauri/core/src/text.rs so the composer
// shows the same numbers the backend validates against.

import type { CountMode, Limits } from "./types";

const URL_RE = /https?:\/\/[^\s<>"'`]+/g;
const REMOTE_MENTION_RE = /(^|[^\w/@])(@[A-Za-z0-9_]+)@[A-Za-z0-9.-]+[A-Za-z0-9]/g;

const segmenter = typeof Intl !== "undefined" && "Segmenter" in Intl ? new Intl.Segmenter(undefined, { granularity: "grapheme" }) : null;

export function graphemes(text: string): number {
  if (!segmenter) return Array.from(text).length;
  let n = 0;
  for (const _ of segmenter.segment(text)) n++;
  return n;
}

function graphemeList(text: string): string[] {
  if (!segmenter) return Array.from(text);
  return Array.from(segmenter.segment(text), (s) => s.segment);
}

/** Links in `text` as [start, end] UTF-16 offsets, without trailing punctuation. */
export function findUrls(text: string): [number, number][] {
  const out: [number, number][] = [];
  for (const m of text.matchAll(URL_RE)) {
    let s = m[0];
    for (;;) {
      const before = s.length;
      s = s.replace(/[.,;:!?'"…]+$/u, "");
      for (const [open, close] of [["(", ")"], ["[", "]"], ["{", "}"]]) {
        if (s.endsWith(close) && s.split(close).length > s.split(open).length) s = s.slice(0, -1);
      }
      if (s.length === before) break;
    }
    const afterScheme = s.slice(s.indexOf("://") + 3);
    if (s.length <= "https://".length || (!afterScheme.includes(".") && !s.includes("localhost"))) continue;
    out.push([m.index!, m.index! + s.length]);
  }
  return out;
}

export function shortUrl(url: string): string {
  const rest = url.replace(/^https?:\/\//, "");
  const chars = Array.from(rest);
  return chars.length <= 30 ? rest : chars.slice(0, 27).join("") + "...";
}

export function shortenLinks(text: string): string {
  let out = "";
  let last = 0;
  for (const [s, e] of findUrls(text)) {
    out += text.slice(last, s) + shortUrl(text.slice(s, e));
    last = e;
  }
  return out + text.slice(last);
}

function twitterLight(cp: number): boolean {
  return cp <= 4351 || (cp >= 8192 && cp <= 8205) || (cp >= 8208 && cp <= 8223) || (cp >= 8242 && cp <= 8247);
}

function isEmoji(g: string): boolean {
  for (const ch of g) {
    const cp = ch.codePointAt(0)!;
    if ((cp >= 0x1f000 && cp <= 0x1faff) || cp === 0xfe0f || cp === 0x200d || (cp >= 0x2600 && cp <= 0x27bf)) return true;
  }
  return false;
}

function twitterWeight(text: string): number {
  const weigh = (seg: string) =>
    graphemeList(seg).reduce((sum, g) => {
      if (isEmoji(g)) return sum + 2;
      let w = 0;
      for (const ch of g) w += twitterLight(ch.codePointAt(0)!) ? 1 : 2;
      return sum + w;
    }, 0);
  let total = 0;
  let last = 0;
  for (const [s, e] of findUrls(text)) {
    total += weigh(text.slice(last, s)) + 23;
    last = e;
  }
  return total + weigh(text.slice(last));
}

export function count(text: string, mode: CountMode): number {
  switch (mode) {
    case "graphemes":
      return graphemes(text);
    case "utf16":
      return text.length;
    case "mastodon": {
      let out = "";
      let last = 0;
      for (const [s, e] of findUrls(text)) {
        out += text.slice(last, s) + "x".repeat(23);
        last = e;
      }
      out += text.slice(last);
      out = out.replace(REMOTE_MENTION_RE, "$1$2");
      return graphemes(out);
    }
    case "bluesky":
      return graphemes(shortenLinks(text));
    case "twitter":
      return twitterWeight(text);
  }
}

/** The character limit that applies, given whether images are attached. */
export function limitFor(limits: Limits, hasMedia: boolean): number {
  return hasMedia && limits.maxCharsWithMedia != null ? limits.maxCharsWithMedia : limits.maxChars;
}

export function preview(text: string, max = 120): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > max ? flat.slice(0, max - 1) + "…" : flat;
}

export function formatCount(n: number | null | undefined): string {
  if (n == null) return "";
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1).replace(/\.0$/, "") + "M";
  if (n >= 10_000) return Math.round(n / 1000) + "K";
  if (n >= 1000) return (n / 1000).toFixed(1).replace(/\.0$/, "") + "K";
  return String(n);
}

export function humanBytes(n: number): string {
  if (n >= 1024 * 1024) return (n / (1024 * 1024)).toFixed(1) + " MB";
  return Math.ceil(n / 1024) + " KB";
}
