// Post HTML from the networks is untrusted. Only a small set of formatting
// tags survives, links get rel/target attributes, and images are limited to
// custom emoji over https.

import DOMPurify from "dompurify";

const ALLOWED_TAGS = ["p", "br", "a", "span", "strong", "b", "em", "i", "u", "del", "s", "code", "pre", "blockquote", "ul", "ol", "li", "img"];
const ALLOWED_ATTR = ["href", "class", "src", "alt", "title"];

DOMPurify.addHook("afterSanitizeAttributes", (node) => {
  if (node.tagName === "A") {
    const href = node.getAttribute("href") || "";
    if (!/^https?:\/\//i.test(href)) node.removeAttribute("href");
    node.setAttribute("rel", "noopener noreferrer");
  }
  if (node.tagName === "IMG") {
    const src = node.getAttribute("src") || "";
    const cls = node.getAttribute("class") || "";
    if (!/^https:\/\//i.test(src) || !cls.split(/\s+/).includes("emoji")) node.remove();
  }
  if (node.hasAttribute("class")) {
    // Keep only the classes Mastodon uses to shorten links.
    const kept = (node.getAttribute("class") || "")
      .split(/\s+/)
      .filter((c) => ["invisible", "ellipsis", "emoji", "mention", "hashtag", "h-card", "u-url"].includes(c));
    if (kept.length) node.setAttribute("class", kept.join(" "));
    else node.removeAttribute("class");
  }
});

export function sanitizePostHtml(html: string): string {
  return DOMPurify.sanitize(html, { ALLOWED_TAGS, ALLOWED_ATTR, ALLOW_DATA_ATTR: false });
}

const LINK_RE = /(https?:\/\/[^\s<>"']+[^\s<>"'.,;:!?)\]])/g;

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

/** Plain text to safe HTML with clickable links and line breaks. */
export function linkify(text: string): string {
  const parts = text.split(LINK_RE);
  const html = parts
    .map((part, i) => (i % 2 === 1 ? `<a href="${escapeHtml(part)}">${escapeHtml(part)}</a>` : escapeHtml(part)))
    .join("")
    .replace(/\n/g, "<br>");
  return sanitizePostHtml(`<p>${html}</p>`);
}
