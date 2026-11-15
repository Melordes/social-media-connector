// The post composer: write once, publish or schedule to several accounts,
// with per-account text, images with descriptions, and live checks.

import { useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { CalendarClock, ImagePlus, Info, Send, SlidersHorizontal, X as XIcon } from "lucide-react";
import type { Account, MediaAttachment, PlatformInfo, PostOptions, ReplyRef, SavePostInput, TargetProblem } from "../lib/types";
import { api, errorMessage } from "../lib/api";
import { replyRef, useApp, type ComposerSeed } from "../lib/store";
import { count, limitFor, preview } from "../lib/text";
import { addDays, fmt, fromInputs, nextSlot, relativeFuture, toDateInput, toTimeInput } from "../lib/format";
import { AccountAvatar, Confirm, Modal, PlatformIcon } from "./common";

const IMAGE_EXT = ["jpg", "jpeg", "png", "gif", "webp"];

const LANGUAGES: [string, string][] = [
  ["", "Not set"],
  ["en", "English"],
  ["de", "German"],
  ["es", "Spanish"],
  ["fr", "French"],
  ["it", "Italian"],
  ["nl", "Dutch"],
  ["pt", "Portuguese"],
  ["sv", "Swedish"],
  ["da", "Danish"],
  ["no", "Norwegian"],
  ["fi", "Finnish"],
  ["pl", "Polish"],
  ["cs", "Czech"],
  ["uk", "Ukrainian"],
  ["ru", "Russian"],
  ["tr", "Turkish"],
  ["ar", "Arabic"],
  ["hi", "Hindi"],
  ["ja", "Japanese"],
  ["ko", "Korean"],
  ["zh", "Chinese"],
];

function mentionPrefix(platform: Account["platform"], handle: string): string {
  // Fediverse replies notify people through mentions in the text.
  return platform === "mastodon" || platform === "misskey" ? `${handle} ` : "";
}

function cleanOptions(o: PostOptions): PostOptions {
  const s = (v: string | null | undefined) => (v && v.trim() ? v.trim() : null);
  return {
    contentWarning: s(o.contentWarning),
    visibility: s(o.visibility),
    sensitive: !!o.sensitive,
    language: s(o.language),
    title: s(o.title),
    subreddit: s(o.subreddit)?.replace(/^\/?r\//i, "") ?? null,
    link: s(o.link),
    tags: (o.tags ?? []).map((t) => t.trim().replace(/^#/, "")).filter(Boolean),
    silent: !!o.silent,
    spoiler: !!o.spoiler,
  };
}

export function Composer({ seed }: { seed: ComposerSeed }) {
  const accounts = useApp((s) => s.accounts);
  const info = useApp((s) => s.info);
  const settings = useApp((s) => s.settings);
  const close = useApp((s) => s.closeComposer);
  const toast = useApp((s) => s.toast);
  const setPage = useApp((s) => s.setPage);
  const openConnect = useApp((s) => s.openConnect);

  // `source` prefills the form; `editing` is set only when changing an existing post
  // (a duplicate arrives as a post without an id).
  const source = seed.post ?? null;
  const editing = source && source.id ? source : null;
  const reply: ReplyRef | null = useMemo(() => {
    if (seed.replyTo) return replyRef(seed.replyTo.item, seed.replyTo.account);
    return source?.replyTo ?? null;
  }, [seed.replyTo, source]);

  const [selected, setSelected] = useState<string[]>(() => {
    if (reply) return [reply.accountId];
    if (source) return source.targets.map((t) => t.accountId).filter((id) => accounts.some((a) => a.id === id));
    if (seed.accountIds) return seed.accountIds;
    const last = (settings?.lastAccounts ?? []).filter((id) => accounts.some((a) => a.id === id));
    return last.length ? last : accounts.length === 1 ? [accounts[0].id] : [];
  });
  const [body, setBody] = useState<string>(() => {
    if (source) return source.body;
    if (seed.replyTo) return mentionPrefix(seed.replyTo.account.platform, seed.replyTo.item.author.handle);
    return "";
  });
  const [overrides, setOverrides] = useState<Record<string, string>>(() => {
    const o: Record<string, string> = {};
    source?.targets.forEach((t) => {
      if (t.bodyOverride != null) o[t.accountId] = t.bodyOverride;
    });
    return o;
  });
  const [customize, setCustomize] = useState(() => Object.keys(overrides).length > 0);
  const [tab, setTab] = useState<string>("all");
  const [media, setMedia] = useState<MediaAttachment[]>(source?.media ?? []);
  const [options, setOptions] = useState<PostOptions>(source?.options ?? {});
  const [tagsText, setTagsText] = useState((source?.options.tags ?? []).join(", "));
  const initialWhen = editing?.scheduledAt ? new Date(editing.scheduledAt) : seed.scheduledAt ? new Date(seed.scheduledAt) : nextSlot();
  const [mode, setMode] = useState<"now" | "schedule">(editing?.status === "scheduled" || seed.scheduledAt ? "schedule" : "now");
  const [date, setDate] = useState(toDateInput(initialWhen));
  const [time, setTime] = useState(toTimeInput(initialWhen));
  const [problems, setProblems] = useState<TargetProblem[]>([]);
  const [busy, setBusy] = useState<null | "draft" | "schedule" | "publish">(null);
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const textRef = useRef<HTMLTextAreaElement>(null);

  const selectedAccounts = selected.map((id) => accounts.find((a) => a.id === id)).filter((a): a is Account => !!a);
  const platformsOf = (list: Account[]) =>
    Array.from(new Set(list.map((a) => a.platform)))
      .map((p) => info?.platforms.find((x) => x.id === p))
      .filter((p): p is PlatformInfo => !!p);
  const platforms = platformsOf(selectedAccounts);
  const has = (pred: (p: PlatformInfo) => boolean) => platforms.some(pred);
  const textFor = (id: string) => (customize && overrides[id] != null ? overrides[id] : body);
  const scheduledAt = mode === "schedule" ? fromInputs(date, time) : null;

  useEffect(() => {
    textRef.current?.focus();
    const el = textRef.current;
    if (el) el.setSelectionRange(el.value.length, el.value.length);
  }, []);

  // Drag and drop of image files onto the window.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        const p = event.payload;
        if (p.type === "enter" || p.type === "over") setDragOver(true);
        else if (p.type === "leave") setDragOver(false);
        else if (p.type === "drop") {
          setDragOver(false);
          const paths = p.paths.filter((x) => IMAGE_EXT.includes(x.split(".").pop()?.toLowerCase() ?? ""));
          if (paths.length) void addPaths(paths);
          else if (p.paths.length) toast("Only images (JPEG, PNG, GIF, WebP) can be attached.", "error");
        }
      })
      .then((fn) => (unlisten = fn));
    return () => unlisten?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const buildInput = (action: SavePostInput["action"]): SavePostInput => ({
    id: editing?.id ?? null,
    body,
    media,
    options: cleanOptions({ ...options, tags: tagsText.split(",") }),
    replyTo: reply,
    targets: selected.map((id) => ({ accountId: id, bodyOverride: customize && overrides[id] != null ? overrides[id] : null })),
    action,
    scheduledAt: scheduledAt ? scheduledAt.getTime() : null,
  });

  // Ask the backend what would stop the post going out (debounced).
  useEffect(() => {
    if (!selected.length) {
      setProblems([]);
      return;
    }
    const t = setTimeout(() => {
      api
        .validatePost(buildInput("publish"))
        .then(setProblems)
        .catch(() => setProblems([]));
    }, 350);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, body, overrides, customize, media, options, tagsText]);

  async function addPaths(paths: string[]) {
    try {
      const added = await api.importMedia(paths);
      setMedia((m) => [...m, ...added]);
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  }

  const pickImages = async () => {
    const picked = await openDialog({ multiple: true, filters: [{ name: "Images", extensions: IMAGE_EXT }] });
    if (!picked) return;
    await addPaths(Array.isArray(picked) ? picked : [picked]);
  };

  const onPaste = async (e: React.ClipboardEvent) => {
    const files = Array.from(e.clipboardData.items)
      .filter((i) => i.kind === "file" && i.type.startsWith("image/"))
      .map((i) => i.getAsFile())
      .filter((f): f is File => !!f);
    if (!files.length) return;
    e.preventDefault();
    for (const f of files) {
      try {
        const bytes = new Uint8Array(await f.arrayBuffer());
        const att = await api.importMediaBytes(f.name || "pasted-image.png", bytes);
        setMedia((m) => [...m, att]);
      } catch (err) {
        toast(errorMessage(err), "error");
      }
    }
  };

  const toggleAccount = (id: string) => {
    if (reply) return;
    setSelected((s) => (s.includes(id) ? s.filter((x) => x !== id) : [...s, id]));
  };

  const problemsFor = (id: string) => problems.find((p) => p.accountId === id)?.problems ?? [];
  const anyProblems = selected.some((id) => problemsFor(id).length > 0);
  const dirty = body.trim() !== "" || media.length > 0;

  const submit = async (action: SavePostInput["action"]) => {
    setError(null);
    if (action !== "draft" && !selected.length) {
      setError("Choose at least one account.");
      return;
    }
    if (action === "schedule" && !scheduledAt) {
      setError("Enter a valid date and time.");
      return;
    }
    setBusy(action);
    try {
      const post = await api.savePost(buildInput(action));
      if (action === "draft") toast("Draft saved.", "success");
      else if (action === "schedule") toast(`Scheduled for ${fmt.dateTime(post.scheduledAt!)}.`, "success");
      else if (post.status === "partial") {
        const failed = post.targets.filter((t) => t.status === "failed");
        toast(`Posted, but ${failed.length} account(s) failed:\n${failed.map((t) => `${t.accountLabel}: ${t.error}`).join("\n")}`, "error");
      } else toast(reply ? "Reply sent." : `Posted to ${post.targets.length} account${post.targets.length === 1 ? "" : "s"}.`, "success");
      close();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const requestClose = () => {
    if (busy) return;
    if (dirty && !editing) setConfirmDiscard(true);
    else close();
  };

  const activeText = tab === "all" ? body : textFor(tab);
  const setActiveText = (v: string) => {
    if (tab === "all") setBody(v);
    else setOverrides((o) => ({ ...o, [tab]: v }));
  };
  const counterAccounts = tab === "all" ? selectedAccounts : selectedAccounts.filter((a) => a.id === tab);
  const strictest = counterAccounts
    .map((a) => {
      const limit = limitFor(a.limits, media.length > 0);
      const used = count(tab === "all" ? textFor(a.id) : activeText, a.limits.countMode);
      return { a, limit, left: limit - used };
    })
    .sort((x, y) => x.left - y.left)[0];

  const visChoices = (() => {
    const fedi = has((p) => p.id === "mastodon" || p.id === "misskey");
    const li = has((p) => p.id === "linkedin");
    if (!fedi && !li) return null;
    const list = [{ value: "public", label: li && !fedi ? "Anyone" : "Public" }];
    if (fedi) list.push({ value: "unlisted", label: "Quiet public (Mastodon, Misskey)" });
    list.push({ value: "followers", label: li && fedi ? "Followers / connections only" : li ? "Connections only" : "Followers only" });
    return list;
  })();
  const titleMode = has((p) => p.features.title === "required") ? "required" : has((p) => p.features.title === "optional") ? "optional" : null;

  const title = reply ? `Reply to ${reply.authorHandle}` : editing ? "Edit post" : source ? "Duplicate post" : "Create post";
  const postable = accounts;

  return (
    <Modal title={title} onClose={requestClose} size="wide" closeOnOverlay={false} icon={<Send size={18} />}>
      <div className="composer" style={{ position: "relative" }}>
        {dragOver && <div className="drop-hint">Drop images to attach them</div>}
        <div className="composer-main">
          {reply && (
            <div className="reply-box">
              <div className="small muted">Replying to {reply.authorHandle}</div>
              <div>{preview(reply.textPreview, 240) || <em className="muted">(no text)</em>}</div>
            </div>
          )}
          <div className="field">
            <span className="field-label">{reply ? "Reply from" : "Post to"}</span>
            {postable.length === 0 ? (
              <div className="info-box">
                No accounts connected yet.{" "}
                <a
                  href="#"
                  onClick={(e) => {
                    e.preventDefault();
                    close();
                    setPage("accounts");
                    openConnect();
                  }}
                >
                  Connect one
                </a>
                .
              </div>
            ) : (
              <div className="picker" role="group" aria-label="Accounts">
                {(reply ? postable.filter((a) => a.id === reply.accountId) : postable).map((a) => (
                  <button
                    key={a.id}
                    className={"picker-chip" + (selected.includes(a.id) ? " selected" : "")}
                    onClick={() => toggleAccount(a.id)}
                    aria-pressed={selected.includes(a.id)}
                    disabled={!!reply}
                    title={`${a.displayName} (${a.handle})${a.status !== "ok" ? " — needs reconnecting" : ""}`}
                  >
                    <AccountAvatar account={a} size={26} />
                    <span>{a.displayName}</span>
                  </button>
                ))}
              </div>
            )}
          </div>

          {customize && selectedAccounts.length > 0 && (
            <div className="tabs" role="tablist">
              <button className={"tab" + (tab === "all" ? " active" : "")} role="tab" aria-selected={tab === "all"} onClick={() => setTab("all")}>
                All accounts
              </button>
              {selectedAccounts.map((a) => (
                <button
                  key={a.id}
                  className={"tab" + (tab === a.id ? " active" : "")}
                  role="tab"
                  aria-selected={tab === a.id}
                  onClick={() => setTab(a.id)}
                >
                  <PlatformIcon platform={a.platform} size={16} />
                  {a.displayName}
                  {overrides[a.id] != null && <span className="pill accent" style={{ padding: "0 6px" }}>edited</span>}
                </button>
              ))}
            </div>
          )}

          <div className="editor">
            <textarea
              ref={textRef}
              value={activeText}
              onChange={(e) => setActiveText(e.target.value)}
              onPaste={(e) => void onPaste(e)}
              placeholder={reply ? "Write your reply…" : "What would you like to share?"}
              aria-label="Post text"
              rows={7}
            />
            <div className="editor-toolbar">
              <button className="btn ghost small" onClick={() => void pickImages()} title="Add images">
                <ImagePlus size={16} /> Images
              </button>
              {!reply && selectedAccounts.length > 1 && (
                <button
                  className={"btn ghost small" + (customize ? " active" : "")}
                  onClick={() => {
                    setCustomize((c) => !c);
                    setTab("all");
                  }}
                  title="Write different text for some accounts"
                  aria-pressed={customize}
                >
                  <SlidersHorizontal size={15} /> {customize ? "Same text for all" : "Customize per account"}
                </button>
              )}
              {tab !== "all" && overrides[tab] != null && (
                <button
                  className="btn ghost small"
                  onClick={() =>
                    setOverrides((o) => {
                      const n = { ...o };
                      delete n[tab];
                      return n;
                    })
                  }
                >
                  Use the main text
                </button>
              )}
              <span className="grow" />
              {strictest && (
                <span
                  className={"counter" + (strictest.left < 0 ? " over" : strictest.left < 20 ? " near" : "")}
                  title={`Tightest limit: ${strictest.a.displayName} (${strictest.limit})`}
                >
                  {strictest.left}
                </span>
              )}
            </div>
          </div>

          {media.length > 0 && (
            <div className="media-strip">
              {media.map((m, i) => (
                <div className="media-thumb" key={m.id}>
                  <div className="thumb-img">
                    <img src={convertFileSrc(m.path)} alt={m.alt || m.fileName} />
                    <button className="remove" aria-label={`Remove ${m.fileName}`} onClick={() => setMedia((list) => list.filter((x) => x.id !== m.id))}>
                      <XIcon size={14} />
                    </button>
                  </div>
                  <textarea
                    placeholder="Describe this image (alt text)"
                    aria-label={`Description for image ${i + 1}`}
                    value={m.alt}
                    onChange={(e) => setMedia((list) => list.map((x) => (x.id === m.id ? { ...x, alt: e.target.value } : x)))}
                  />
                </div>
              ))}
            </div>
          )}

          {platforms.length > 0 && (
            <div className="options">
              {!reply && (titleMode || has((p) => p.features.subreddit) || has((p) => p.features.link)) && (
                <div className="option-group">
                  <h4>
                    {has((p) => p.id === "reddit") && <PlatformIcon platform="reddit" size={16} />}
                    {has((p) => p.id === "tumblr") && <PlatformIcon platform="tumblr" size={16} />}
                    {has((p) => p.id === "facebook") && <PlatformIcon platform="facebook" size={16} />}
                    Title and link
                  </h4>
                  {titleMode && (
                    <div className="field">
                      <label htmlFor="opt-title">Title {titleMode === "required" ? "(required for Reddit)" : "(optional)"}</label>
                      <input id="opt-title" className="input" value={options.title ?? ""} maxLength={300} onChange={(e) => setOptions({ ...options, title: e.target.value })} />
                    </div>
                  )}
                  {has((p) => p.features.subreddit) && (
                    <div className="field">
                      <label htmlFor="opt-sub">Subreddit</label>
                      <input
                        id="opt-sub"
                        className="input"
                        placeholder="e.g. r/test, or u_yourname for your profile"
                        value={options.subreddit ?? ""}
                        onChange={(e) => setOptions({ ...options, subreddit: e.target.value })}
                      />
                    </div>
                  )}
                  {has((p) => p.features.link) && (
                    <div className="field">
                      <label htmlFor="opt-link">Link (optional)</label>
                      <input id="opt-link" className="input" placeholder="https://" value={options.link ?? ""} onChange={(e) => setOptions({ ...options, link: e.target.value })} />
                      {has((p) => p.id === "reddit") && <span className="help">With a link, Reddit gets a link post; the text is not sent with it.</span>}
                    </div>
                  )}
                </div>
              )}
              {(has((p) => p.features.contentWarning) || visChoices || has((p) => p.features.language) || has((p) => p.features.sensitive)) && (
                <div className="option-group">
                  <h4>Audience and content</h4>
                  {visChoices && (
                    <div className="field">
                      <label htmlFor="opt-vis">Who can see it</label>
                      <select id="opt-vis" className="select" value={options.visibility ?? "public"} onChange={(e) => setOptions({ ...options, visibility: e.target.value })}>
                        {visChoices.map((c) => (
                          <option key={c.value} value={c.value}>
                            {c.label}
                          </option>
                        ))}
                      </select>
                    </div>
                  )}
                  {has((p) => p.features.contentWarning) && (
                    <div className="field">
                      <label htmlFor="opt-cw">Content warning (Mastodon, Misskey)</label>
                      <input
                        id="opt-cw"
                        className="input"
                        placeholder="Optional: the post is hidden behind this text"
                        value={options.contentWarning ?? ""}
                        onChange={(e) => setOptions({ ...options, contentWarning: e.target.value })}
                      />
                    </div>
                  )}
                  {has((p) => p.features.language) && (
                    <div className="field">
                      <label htmlFor="opt-lang">Language</label>
                      <select id="opt-lang" className="select" value={options.language ?? ""} onChange={(e) => setOptions({ ...options, language: e.target.value })}>
                        {LANGUAGES.map(([v, l]) => (
                          <option key={v} value={v}>
                            {l}
                          </option>
                        ))}
                      </select>
                    </div>
                  )}
                  {has((p) => p.features.sensitive) && (
                    <label className="check">
                      <input type="checkbox" checked={!!options.sensitive} onChange={(e) => setOptions({ ...options, sensitive: e.target.checked })} />
                      <span>Mark as sensitive (hides images on Mastodon and Misskey; NSFW on Reddit)</span>
                    </label>
                  )}
                  {has((p) => p.features.spoiler) && (
                    <label className="check">
                      <input type="checkbox" checked={!!options.spoiler} onChange={(e) => setOptions({ ...options, spoiler: e.target.checked })} />
                      <span>Mark as spoiler (Reddit)</span>
                    </label>
                  )}
                </div>
              )}
              {!reply && (has((p) => p.features.tags) || has((p) => p.features.silent)) && (
                <div className="option-group">
                  <h4>More options</h4>
                  {has((p) => p.features.tags) && (
                    <div className="field">
                      <label htmlFor="opt-tags">Tags (Tumblr)</label>
                      <input id="opt-tags" className="input" placeholder="art, photography" value={tagsText} onChange={(e) => setTagsText(e.target.value)} />
                    </div>
                  )}
                  {has((p) => p.features.silent) && (
                    <label className="check">
                      <input type="checkbox" checked={!!options.silent} onChange={(e) => setOptions({ ...options, silent: e.target.checked })} />
                      <span>Send silently (Telegram subscribers get no sound)</span>
                    </label>
                  )}
                </div>
              )}
            </div>
          )}
        </div>

        <aside className="composer-side">
          <div>
            <div className="field-label" style={{ marginBottom: 6 }}>
              Checks
            </div>
            {selectedAccounts.length === 0 && <div className="small muted">Select accounts to see their limits.</div>}
            {selectedAccounts.map((a) => {
              const limit = limitFor(a.limits, media.length > 0);
              const used = count(textFor(a.id), a.limits.countMode);
              const left = limit - used;
              // Don't shout about an empty post before anything was written.
              const probs = problemsFor(a.id).filter((p) => !(p === "The post is empty." && !textFor(a.id).trim() && !media.length));
              return (
                <div className="target-row" key={a.id}>
                  <AccountAvatar account={a} size={28} />
                  <div className="grow">
                    <div className="row" style={{ gap: 6 }}>
                      <strong className="grow" style={{ whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", fontSize: 13 }}>
                        {a.displayName}
                      </strong>
                      <span className={"counter" + (left < 0 ? " over" : left < 20 ? " near" : "")}>
                        {used}/{limit}
                      </span>
                    </div>
                    {a.status !== "ok" && <div className="problem">Needs reconnecting before it can post.</div>}
                    {probs.map((p, i) => (
                      <div className="problem" key={i}>
                        {p}
                      </div>
                    ))}
                    {probs.length === 0 && a.status === "ok" && (
                      <div className="small faint">
                        {!textFor(a.id).trim() && !media.length ? "Waiting for text" : media.length ? `${media.length}/${a.limits.maxImages} images` : "Ready"}
                      </div>
                    )}
                  </div>
                </div>
              );
            })}
          </div>

          {!reply && (
            <div>
              <div className="field-label" style={{ marginBottom: 8 }}>
                When
              </div>
              <div className="segmented" role="radiogroup" aria-label="When to publish">
                <button className={mode === "now" ? "active" : ""} onClick={() => setMode("now")} role="radio" aria-checked={mode === "now"}>
                  Post now
                </button>
                <button className={mode === "schedule" ? "active" : ""} onClick={() => setMode("schedule")} role="radio" aria-checked={mode === "schedule"}>
                  Schedule
                </button>
              </div>
              {mode === "schedule" && (
                <div style={{ marginTop: 12 }}>
                  <div className="row">
                    <input type="date" className="input" value={date} onChange={(e) => setDate(e.target.value)} aria-label="Date" />
                    <input type="time" className="input" value={time} onChange={(e) => setTime(e.target.value)} aria-label="Time" style={{ width: 120 }} />
                  </div>
                  <div className="small muted" style={{ marginTop: 6 }}>
                    {scheduledAt ? `${fmt.dateTime(scheduledAt)} (${relativeFuture(scheduledAt.getTime())})` : "Enter a date and time."}
                  </div>
                  <div className="quick-picks">
                    {[
                      ["In 1 hour", new Date(Date.now() + 3600_000)],
                      ["Tomorrow 9:00", (() => {
                        const d = addDays(new Date(), 1);
                        d.setHours(9, 0, 0, 0);
                        return d;
                      })()],
                      ["Monday 9:00", (() => {
                        const d = new Date();
                        const add = ((8 - d.getDay()) % 7) || 7;
                        const n = addDays(d, add);
                        n.setHours(9, 0, 0, 0);
                        return n;
                      })()],
                    ].map(([label, d]) => (
                      <button
                        key={label as string}
                        className="btn small"
                        onClick={() => {
                          setDate(toDateInput(d as Date));
                          setTime(toTimeInput(d as Date));
                        }}
                      >
                        {label as string}
                      </button>
                    ))}
                  </div>
                  <div className="info-box small" style={{ marginTop: 10, display: "flex", gap: 8 }}>
                    <Info size={15} style={{ flex: "none", marginTop: 1 }} />
                    <span>Scheduled posts are sent by this app, so keep it running (it can stay in the tray).</span>
                  </div>
                </div>
              )}
            </div>
          )}
          {error && <div className="error-box">{error}</div>}
        </aside>
      </div>
      <div className="modal-footer">
        <button className="btn" onClick={requestClose} disabled={!!busy}>
          Cancel
        </button>
        <span className="spacer" />
        {!reply && (
          <button className="btn" onClick={() => void submit("draft")} disabled={!!busy || (!body.trim() && !media.length)}>
            {busy === "draft" ? "Saving…" : "Save draft"}
          </button>
        )}
        {mode === "schedule" && !reply ? (
          <button
            className="btn primary"
            onClick={() => void submit("schedule")}
            disabled={!!busy || !selected.length || anyProblems || !scheduledAt}
            title={anyProblems ? "Fix the problems listed on the right first" : undefined}
          >
            <CalendarClock size={16} /> {busy === "schedule" ? "Scheduling…" : editing?.status === "scheduled" ? "Update schedule" : "Schedule"}
          </button>
        ) : (
          <button
            className="btn primary"
            onClick={() => void submit("publish")}
            disabled={!!busy || !selected.length || anyProblems}
            title={anyProblems ? "Fix the problems listed on the right first" : undefined}
          >
            <Send size={15} /> {busy === "publish" ? "Posting…" : reply ? "Send reply" : "Post now"}
          </button>
        )}
      </div>
      {confirmDiscard && (
        <Confirm
          title="Discard this post?"
          message="What you wrote will be lost. Save it as a draft to keep it."
          confirmLabel="Discard"
          danger
          onConfirm={() => {
            setConfirmDiscard(false);
            close();
          }}
          onCancel={() => setConfirmDiscard(false)}
        />
      )}
    </Modal>
  );
}
