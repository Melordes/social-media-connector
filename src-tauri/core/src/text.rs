//! Text helpers: character counting the way each network counts, link and
//! mention detection, and HTML to plain text.
//!
//! The frontend mirrors `count` in `src/lib/text.ts`; keep the two in sync.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

/// How a network counts the length of a post.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CountMode {
    /// User-perceived characters (grapheme clusters).
    Graphemes,
    /// Graphemes, with each link counted as 23 and `@user@domain` as `@user`.
    Mastodon,
    /// Graphemes after shortening long links the way the Bluesky app does.
    Bluesky,
    /// X's weighted count: links are 23, most non-Latin characters and emoji 2.
    Twitter,
    /// UTF-16 code units (Telegram, Discord).
    Utf16,
}

pub static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s<>"'`]+"#).expect("url regex")
});

static REMOTE_MENTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|[^\w/@])(@[A-Za-z0-9_]+)@[A-Za-z0-9.\-]+[A-Za-z0-9]").expect("mention regex")
});

/// Finds links in `text` as byte ranges, without trailing punctuation.
pub fn find_urls(text: &str) -> Vec<(usize, usize)> {
    URL_RE
        .find_iter(text)
        .filter_map(|m| {
            let mut end = m.end();
            let s = &text[m.start()..end];
            let mut trimmed = s;
            loop {
                let before = trimmed.len();
                trimmed = trimmed.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"', '…']);
                // Drop a closing bracket only when it is not balanced inside the URL.
                for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
                    if trimmed.ends_with(close)
                        && trimmed.matches(close).count() > trimmed.matches(open).count()
                    {
                        trimmed = &trimmed[..trimmed.len() - close.len_utf8()];
                    }
                }
                if trimmed.len() == before {
                    break;
                }
            }
            end = m.start() + trimmed.len();
            // "https://" alone is not a link.
            if trimmed.len() <= "https://".len() || !trimmed[trimmed.find("://")? + 3..].contains('.') && !trimmed.contains("localhost") {
                return None;
            }
            Some((m.start(), end))
        })
        .collect()
}

pub fn graphemes(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Length of `text` as the given network counts it.
pub fn count(text: &str, mode: CountMode) -> usize {
    match mode {
        CountMode::Graphemes => graphemes(text),
        CountMode::Utf16 => text.encode_utf16().count(),
        CountMode::Mastodon => {
            let mut out = String::with_capacity(text.len());
            let mut last = 0;
            for (s, e) in find_urls(text) {
                out.push_str(&text[last..s]);
                out.push_str(&"x".repeat(23));
                last = e;
            }
            out.push_str(&text[last..]);
            let out = REMOTE_MENTION_RE.replace_all(&out, "$1$2");
            graphemes(&out)
        }
        CountMode::Bluesky => graphemes(&shorten_links(text).0),
        CountMode::Twitter => twitter_weight(text),
    }
}

fn twitter_light(c: char) -> bool {
    let c = c as u32;
    c <= 4351 || (8192..=8205).contains(&c) || (8208..=8223).contains(&c) || (8242..=8247).contains(&c)
}

fn is_emoji_grapheme(g: &str) -> bool {
    g.chars().any(|c| {
        let c = c as u32;
        (0x1F000..=0x1FAFF).contains(&c) || c == 0xFE0F || c == 0x200D || (0x2600..=0x27BF).contains(&c)
    })
}

fn twitter_weight(text: &str) -> usize {
    let mut total = 0;
    let mut last = 0;
    let weigh = |segment: &str| -> usize {
        segment
            .graphemes(true)
            .map(|g| {
                if is_emoji_grapheme(g) {
                    2
                } else {
                    g.chars().map(|c| if twitter_light(c) { 1 } else { 2 }).sum()
                }
            })
            .sum()
    };
    for (s, e) in find_urls(text) {
        total += weigh(&text[last..s]) + 23;
        last = e;
    }
    total + weigh(&text[last..])
}

/// Shortens long links for display the way the Bluesky app does, returning
/// the new text and, for each link, its byte range in the new text plus the
/// full URL it points to.
pub fn shorten_links(text: &str) -> (String, Vec<(usize, usize, String)>) {
    let mut out = String::with_capacity(text.len());
    let mut links = Vec::new();
    let mut last = 0;
    for (s, e) in find_urls(text) {
        out.push_str(&text[last..s]);
        let url = &text[s..e];
        let display = short_url(url);
        let start = out.len();
        out.push_str(&display);
        links.push((start, out.len(), url.to_string()));
        last = e;
    }
    out.push_str(&text[last..]);
    (out, links)
}

/// `https://example.com/a/very/long/path?x=1` → `example.com/a/very/long/pa...`
pub fn short_url(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let chars: Vec<char> = without_scheme.chars().collect();
    if chars.len() <= 30 {
        without_scheme.to_string()
    } else {
        let mut s: String = chars[..27].iter().collect();
        s.push_str("...");
        s
    }
}

static MENTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s(])(@(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)")
        .expect("handle regex")
});

static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)([#＃][^\s#＃]+)").expect("tag regex"));

/// `@handle.domain` mentions in `text` as (byte start, byte end, handle).
pub fn find_handles(text: &str) -> Vec<(usize, usize, String)> {
    MENTION_RE
        .captures_iter(text)
        .filter_map(|c| {
            let m = c.get(1)?;
            let handle = m.as_str().trim_start_matches('@').trim_end_matches('.').to_lowercase();
            // Reject things like "@1.5" that are not handles.
            let tld = handle.rsplit('.').next()?;
            if tld.len() < 2 || tld.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some((m.start(), m.start() + 1 + handle.len(), handle))
        })
        .collect()
}

/// `#tags` in `text` as (byte start, byte end, tag without '#').
pub fn find_tags(text: &str) -> Vec<(usize, usize, String)> {
    TAG_RE
        .captures_iter(text)
        .filter_map(|c| {
            let m = c.get(1)?;
            let raw = m.as_str();
            let hash_len = raw.chars().next()?.len_utf8();
            let body = raw[hash_len..].trim_end_matches(|c: char| c.is_ascii_punctuation() && c != '_');
            if body.is_empty() || body.chars().all(|c| c.is_ascii_digit()) || body.chars().count() > 64 {
                return None;
            }
            Some((m.start(), m.start() + hash_len + body.len(), body.to_string()))
        })
        .collect()
}

/// Converts the small HTML used in posts to plain text.
pub fn html_to_text(html: &str) -> String {
    static BR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
    static P_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</p\s*>|</li\s*>|</h[1-6]\s*>|</blockquote\s*>").unwrap());
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
    static SCRIPT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<(script|style)[^>]*>.*?</(script|style)\s*>").unwrap());
    static MANY_NL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
    let s = SCRIPT.replace_all(html, "");
    let s = BR.replace_all(&s, "\n");
    let s = P_END.replace_all(&s, "\n\n");
    let s = TAG.replace_all(&s, "");
    let s = decode_entities(&s);
    let s = MANY_NL.replace_all(&s, "\n\n");
    s.trim().to_string()
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    static ENTITY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);").unwrap());
    ENTITY
        .replace_all(s, |caps: &regex::Captures| {
            let e = &caps[1];
            let decoded = if let Some(hex) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).map(String::from)
            } else if let Some(dec) = e.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32).map(String::from)
            } else {
                match e {
                    "amp" => Some("&".into()),
                    "lt" => Some("<".into()),
                    "gt" => Some(">".into()),
                    "quot" => Some("\"".into()),
                    "apos" => Some("'".into()),
                    "nbsp" => Some("\u{a0}".into()),
                    "hellip" => Some("…".into()),
                    "mdash" => Some("—".into()),
                    "ndash" => Some("–".into()),
                    "rsquo" => Some("’".into()),
                    "lsquo" => Some("‘".into()),
                    "rdquo" => Some("”".into()),
                    "ldquo" => Some("“".into()),
                    _ => None,
                }
            };
            decoded.unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

/// Escapes text for inclusion in HTML.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A short, single-line preview of a text.
pub fn preview(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::http::truncate(&flat, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls_without_trailing_punctuation() {
        let t = "See https://example.com/a_(b), and (https://x.org/y). Also http://localhost:8080/z!";
        let urls: Vec<&str> = find_urls(t).into_iter().map(|(s, e)| &t[s..e]).collect();
        assert_eq!(urls, vec!["https://example.com/a_(b)", "https://x.org/y", "http://localhost:8080/z"]);
    }

    #[test]
    fn counts_like_each_network() {
        let text = "Hello https://example.com/a/really/long/path/that/goes/on/and/on @alice@example.social 👋🏽";
        // Mastodon: link = 23, remote mention counts the local part only.
        assert_eq!(count(text, CountMode::Mastodon), "Hello ".len() + 23 + " @alice ".len() + 1);
        // Twitter: link = 23, emoji = 2.
        assert_eq!(count("abc https://t.co/x 😀", CountMode::Twitter), 4 + 23 + 1 + 2);
        assert_eq!(count("日本語", CountMode::Twitter), 6);
        // UTF-16: the emoji with skin tone is 4 code units.
        assert_eq!(count("👋🏽", CountMode::Utf16), 4);
        assert_eq!(count("👋🏽", CountMode::Graphemes), 1);
        // Bluesky: long links are shortened to 30 characters.
        assert_eq!(count("https://example.com/a/really/long/path/that/goes/on", CountMode::Bluesky), 30);
    }

    #[test]
    fn shortens_links_with_ranges() {
        let (t, links) = shorten_links("go https://example.com/a/really/long/path/that/goes/on now");
        assert_eq!(t, "go example.com/a/really/long/p... now");
        assert_eq!(links.len(), 1);
        let (s, e, url) = &links[0];
        assert_eq!(&t[*s..*e], "example.com/a/really/long/p...");
        assert_eq!(url, "https://example.com/a/really/long/path/that/goes/on");
    }

    #[test]
    fn finds_handles_and_tags() {
        let t = "hi @alice.bsky.social and (@bob.example.com) not @1.5 #rust #2024 #día!";
        let handles: Vec<String> = find_handles(t).into_iter().map(|h| h.2).collect();
        assert_eq!(handles, vec!["alice.bsky.social", "bob.example.com"]);
        let tags = find_tags(t);
        let names: Vec<&str> = tags.iter().map(|t| t.2.as_str()).collect();
        assert_eq!(names, vec!["rust", "día"]);
        let (s, e, _) = &tags[1];
        assert_eq!(&t[*s..*e], "#día");
    }

    #[test]
    fn converts_html() {
        let html = "<p>Hello <a href=\"https://x\">world</a> &amp; friends</p><p>Line<br/>two &#x1F600;</p>";
        assert_eq!(html_to_text(html), "Hello world & friends\n\nLine\ntwo 😀");
    }
}
