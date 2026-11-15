// Planner: a month calendar and lists of scheduled, draft, sent and failed
// posts, with a detail view to edit, publish, retry or delete.

import { useCallback, useEffect, useMemo, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  CalendarDays,
  ChevronLeft,
  ChevronRight,
  Copy,
  ExternalLink,
  List,
  Pencil,
  Plus,
  RotateCcw,
  Send,
  Trash2,
  Undo2,
} from "lucide-react";
import type { Post, PostStatus, PostTarget } from "../lib/types";
import { api, errorMessage } from "../lib/api";
import { useApp } from "../lib/store";
import { preview } from "../lib/text";
import { dayKey, fmt, monthGrid, nextSlot, relativeFuture, sameDay, startOfDay } from "../lib/format";
import { AccountAvatar, Avatar, Confirm, EmptyState, Modal, PlatformIcon, Spinner } from "../components/common";

const STATUS_LABEL: Record<PostStatus, string> = {
  draft: "Draft",
  scheduled: "Scheduled",
  publishing: "Publishing",
  published: "Published",
  partial: "Partly published",
  failed: "Failed",
  missed: "Missed",
};

const STATUS_PILL: Record<PostStatus, string> = {
  draft: "",
  scheduled: "accent",
  publishing: "accent",
  published: "success",
  partial: "warning",
  failed: "danger",
  missed: "warning",
};

export function postTime(p: Post): number {
  return p.scheduledAt ?? p.publishedAt ?? p.updatedAt * 1000;
}

function TargetAvatars({ post, size = 22 }: { post: Post; size?: number }) {
  const accounts = useApp((s) => s.accounts);
  return (
    <span className="avatars">
      {post.targets.slice(0, 5).map((t) => {
        const a = accounts.find((x) => x.id === t.accountId);
        return a ? (
          <AccountAvatar key={t.accountId} account={a} size={size} />
        ) : (
          <Avatar key={t.accountId} src={null} name={t.accountLabel} size={size} platform={t.platform} title={t.accountLabel} />
        );
      })}
      {post.targets.length > 5 && <span className="small muted" style={{ marginLeft: 6 }}>+{post.targets.length - 5}</span>}
    </span>
  );
}

function TargetRow({ post, target, onChanged }: { post: Post; target: PostTarget; onChanged: (p: Post) => void }) {
  const accounts = useApp((s) => s.accounts);
  const info = useApp((s) => s.info);
  const toast = useApp((s) => s.toast);
  const account = accounts.find((a) => a.id === target.accountId);
  const platform = info?.platforms.find((p) => p.id === target.platform);
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const pill =
    target.status === "published"
      ? ["success", target.error === "Deleted from the network" ? "Deleted" : "Published"]
      : target.status === "failed"
        ? ["danger", "Failed"]
        : target.status === "publishing"
          ? ["accent", "Publishing"]
          : ["", "Waiting"];
  const deleteRemote = async () => {
    setConfirm(false);
    setBusy(true);
    try {
      onChanged(await api.deleteRemote(post.id, target.accountId));
      toast(`Deleted from ${platform?.name}.`, "success");
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="target-row">
      {account ? <AccountAvatar account={account} size={32} /> : <Avatar src={null} name={target.accountLabel} size={32} platform={target.platform} />}
      <div className="grow">
        <div className="row" style={{ gap: 8 }}>
          <strong className="grow" style={{ fontSize: 13 }}>
            {account ? `${account.displayName} ${account.handle}` : `${target.accountLabel} (removed)`}
          </strong>
          <span className={"pill " + pill[0]}>{pill[1]}</span>
        </div>
        {target.bodyOverride != null && <div className="small muted">Custom text: {preview(target.bodyOverride, 140)}</div>}
        {target.error && target.status !== "published" && <div className="problem">{target.error}</div>}
        {target.publishedAt && target.status === "published" && <div className="small faint">{fmt.dateTime(target.publishedAt)}</div>}
        <div className="row" style={{ gap: 6, marginTop: 6 }}>
          {target.remoteUrl && (
            <button className="btn small" onClick={() => void api.openUrl(target.remoteUrl!)}>
              <ExternalLink size={13} /> View
            </button>
          )}
          {target.status === "published" && target.remoteId && platform?.features.delete && account && (
            <button className="btn small danger" disabled={busy} onClick={() => setConfirm(true)}>
              <Trash2 size={13} /> Delete from {platform.name}
            </button>
          )}
        </div>
      </div>
      {confirm && (
        <Confirm
          title={`Delete from ${platform?.name}?`}
          message="The post will be removed from the network for everyone. It stays in your Planner history."
          confirmLabel="Delete"
          danger
          onConfirm={() => void deleteRemote()}
          onCancel={() => setConfirm(false)}
        />
      )}
    </div>
  );
}

export function PostDetail({ post: initial, onClose }: { post: Post; onClose: () => void }) {
  const openComposer = useApp((s) => s.openComposer);
  const toast = useApp((s) => s.toast);
  const postsVersion = useApp((s) => s.postsVersion);
  const [post, setPost] = useState(initial);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);

  useEffect(() => {
    api.getPost(initial.id).then(setPost).catch(() => undefined);
  }, [initial.id, postsVersion]);

  const run = async (label: string, fn: () => Promise<Post | void>, done?: string) => {
    setBusy(label);
    try {
      const result = await fn();
      if (result) setPost(result);
      if (done) toast(done, "success");
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(null);
    }
  };

  const editable = ["draft", "scheduled", "missed", "failed", "partial"].includes(post.status);
  const canPublish = ["draft", "scheduled", "missed"].includes(post.status);
  const canRetry = ["failed", "partial"].includes(post.status);
  const when = postTime(post);

  return (
    <Modal
      title={post.replyTo ? `Reply to ${post.replyTo.authorHandle}` : "Post"}
      onClose={onClose}
      size="medium"
      icon={<span className={"pill " + STATUS_PILL[post.status]}>{STATUS_LABEL[post.status]}</span>}
      footer={
        <>
          <button className="btn danger" onClick={() => setConfirmDelete(true)} disabled={!!busy || post.status === "publishing"}>
            <Trash2 size={15} /> Delete
          </button>
          <button
            className="btn"
            onClick={() => {
              onClose();
              openComposer({ post: { ...post, id: "", status: "draft", scheduledAt: null, targets: post.targets.map((t) => ({ ...t, status: "pending", remoteId: null, remoteUrl: null, error: null })) } });
            }}
          >
            <Copy size={15} /> Duplicate
          </button>
          <span className="spacer" />
          {(post.status === "scheduled" || post.status === "missed") && (
            <button className="btn" disabled={!!busy} onClick={() => void run("unschedule", () => api.unschedulePost(post.id), "Moved to drafts.")}>
              <Undo2 size={15} /> Move to drafts
            </button>
          )}
          {editable && post.status !== "partial" && (
            <button
              className="btn"
              onClick={() => {
                onClose();
                openComposer({ post });
              }}
            >
              <Pencil size={15} /> Edit
            </button>
          )}
          {canRetry && (
            <button className="btn primary" disabled={!!busy} onClick={() => void run("retry", () => api.publishPost(post.id), "Sent again.")}>
              {busy === "retry" ? <Spinner /> : <RotateCcw size={15} />} Retry failed
            </button>
          )}
          {canPublish && (
            <button className="btn primary" disabled={!!busy} onClick={() => void run("publish", () => api.publishPost(post.id), "Published.")}>
              {busy === "publish" ? <Spinner /> : <Send size={15} />} Publish now
            </button>
          )}
        </>
      }
    >
      <div className="modal-body">
        <p className="muted" style={{ marginTop: 0 }}>
          {post.status === "scheduled"
            ? `Scheduled for ${fmt.dateTime(when)} (${relativeFuture(when)})`
            : post.status === "draft"
              ? `Draft, last edited ${fmt.dateTime(post.updatedAt * 1000)}`
              : post.status === "missed"
                ? `Was due ${fmt.dateTime(when)} while the app was not running.`
                : post.publishedAt
                  ? `Sent ${fmt.dateTime(post.publishedAt)}`
                  : `Last attempt ${fmt.dateTime(post.updatedAt * 1000)}`}
        </p>
        {post.options.title && <h3 style={{ margin: "0 0 8px" }}>{post.options.title}</h3>}
        {post.body ? <p className="detail-text">{post.body}</p> : <p className="muted">(No text)</p>}
        {post.media.length > 0 && (
          <div className="thumbs" style={{ marginBottom: 14 }}>
            {post.media.map((m) => (
              <img key={m.id} src={convertFileSrc(m.path)} alt={m.alt || m.fileName} title={m.alt || m.fileName} />
            ))}
          </div>
        )}
        <div className="field-label" style={{ marginBottom: 4 }}>
          Accounts
        </div>
        {post.targets.length === 0 && <p className="small muted">No accounts selected yet.</p>}
        {post.targets.map((t) => (
          <TargetRow key={t.accountId} post={post} target={t} onChanged={setPost} />
        ))}
      </div>
      {confirmDelete && (
        <Confirm
          title="Delete this post from the Planner?"
          message={
            post.targets.some((t) => t.status === "published")
              ? "This only removes it from the app. What was already published stays online; use \"Delete from …\" above to remove it there first."
              : "It will not be published."
          }
          confirmLabel="Delete"
          danger
          onConfirm={() => {
            setConfirmDelete(false);
            void run("delete", async () => {
              await api.deletePost(post.id);
              onClose();
            }, "Post deleted.");
          }}
          onCancel={() => setConfirmDelete(false)}
        />
      )}
    </Modal>
  );
}

function PostRow({ post, onOpen }: { post: Post; onOpen: () => void }) {
  const when = postTime(post);
  return (
    <div className="card post-row" role="button" tabIndex={0} onClick={onOpen} onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && onOpen()}>
      <div className="when">
        <strong>{post.status === "draft" && !post.scheduledAt ? "Draft" : fmt.time(when)}</strong>
        <span className="small muted">{post.status === "draft" && !post.scheduledAt ? fmt.shortDate(post.updatedAt * 1000) : fmt.day(when)}</span>
      </div>
      <div className="body">
        <div className="text">{post.options.title ? <strong>{post.options.title} — </strong> : null}{post.body || <em className="muted">(no text)</em>}</div>
        {post.lastError && (post.status === "failed" || post.status === "partial" || post.status === "missed") && (
          <div className="problem">{preview(post.lastError, 160)}</div>
        )}
        <div className="row" style={{ marginTop: 8 }}>
          <TargetAvatars post={post} />
          {post.media.length > 0 && <span className="small muted">{post.media.length} image{post.media.length > 1 ? "s" : ""}</span>}
        </div>
      </div>
      <span className={"pill " + STATUS_PILL[post.status]}>{STATUS_LABEL[post.status]}</span>
    </div>
  );
}

type ListTab = "scheduled" | "drafts" | "sent" | "problems";
const LIST_STATUSES: Record<ListTab, PostStatus[]> = {
  scheduled: ["scheduled", "publishing"],
  drafts: ["draft"],
  sent: ["published", "partial"],
  problems: ["failed", "missed", "partial"],
};

export function PlannerPage() {
  const settings = useApp((s) => s.settings);
  const postsVersion = useApp((s) => s.postsVersion);
  const openComposer = useApp((s) => s.openComposer);
  const accounts = useApp((s) => s.accounts);
  const toast = useApp((s) => s.toast);
  const [view, setView] = useState<"calendar" | "list">("calendar");
  const [tab, setTab] = useState<ListTab>("scheduled");
  const [month, setMonth] = useState(() => new Date(new Date().getFullYear(), new Date().getMonth(), 1));
  const [posts, setPosts] = useState<Post[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState<Post | null>(null);
  const [dayList, setDayList] = useState<Date | null>(null);
  const [counts, setCounts] = useState<Record<ListTab, number>>({ scheduled: 0, drafts: 0, sent: 0, problems: 0 });

  const grid = useMemo(() => monthGrid(month, settings?.weekStartsMonday ?? true), [month, settings?.weekStartsMonday]);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      if (view === "calendar") {
        const from = grid[0].getTime();
        const to = startOfDay(grid[41]).getTime() + 86400_000;
        setPosts(await api.listPosts([], from, to));
      } else {
        const list = await api.listPosts(LIST_STATUSES[tab]);
        if (tab === "scheduled") list.sort((a, b) => postTime(a) - postTime(b));
        setPosts(list);
      }
      const [s, d, sent, p] = await Promise.all(
        (["scheduled", "drafts", "sent", "problems"] as ListTab[]).map((t) => api.listPosts(LIST_STATUSES[t]).then((l) => l.length)),
      );
      setCounts({ scheduled: s, drafts: d, sent, problems: p });
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setLoading(false);
    }
  }, [view, tab, grid, toast]);

  useEffect(() => {
    void load();
  }, [load, postsVersion]);

  const byDay = useMemo(() => {
    const m = new Map<string, Post[]>();
    for (const p of posts) {
      if (p.status === "draft" && !p.scheduledAt) continue;
      const k = dayKey(new Date(postTime(p)));
      m.set(k, [...(m.get(k) ?? []), p]);
    }
    for (const list of m.values()) list.sort((a, b) => postTime(a) - postTime(b));
    return m;
  }, [posts]);

  const today = new Date();
  const weekdays = grid.slice(0, 7).map((d) => fmt.weekday(d));

  const newOnDay = (d: Date) => {
    if (startOfDay(d) < startOfDay(today)) return;
    const slot = sameDay(d, today) ? nextSlot() : new Date(d.getFullYear(), d.getMonth(), d.getDate(), 9, 0);
    openComposer({ scheduledAt: slot.getTime() });
  };

  return (
    <>
      <header className="page-header">
        <h1>Planner</h1>
        <div className="segmented" role="radiogroup" aria-label="View">
          <button className={view === "calendar" ? "active" : ""} onClick={() => setView("calendar")} role="radio" aria-checked={view === "calendar"}>
            <CalendarDays size={14} style={{ verticalAlign: -2 }} /> Calendar
          </button>
          <button className={view === "list" ? "active" : ""} onClick={() => setView("list")} role="radio" aria-checked={view === "list"}>
            <List size={14} style={{ verticalAlign: -2 }} /> Lists
          </button>
        </div>
        {view === "calendar" && (
          <div className="row" style={{ gap: 4 }}>
            <button className="icon-btn" aria-label="Previous month" onClick={() => setMonth(new Date(month.getFullYear(), month.getMonth() - 1, 1))}>
              <ChevronLeft size={18} />
            </button>
            <strong style={{ minWidth: 150, textAlign: "center" }}>{fmt.month(month)}</strong>
            <button className="icon-btn" aria-label="Next month" onClick={() => setMonth(new Date(month.getFullYear(), month.getMonth() + 1, 1))}>
              <ChevronRight size={18} />
            </button>
            <button className="btn small" onClick={() => setMonth(new Date(today.getFullYear(), today.getMonth(), 1))}>
              Today
            </button>
          </div>
        )}
        {loading && <Spinner />}
        <span className="spacer" />
        {counts.problems > 0 && (
          <button
            className="pill danger"
            style={{ border: "none", cursor: "pointer" }}
            onClick={() => {
              setView("list");
              setTab("problems");
            }}
          >
            {counts.problems} need{counts.problems === 1 ? "s" : ""} attention
          </button>
        )}
        <button className="btn primary" onClick={() => openComposer({ scheduledAt: nextSlot().getTime() })} disabled={!accounts.length}>
          <Plus size={15} /> Schedule a post
        </button>
      </header>
      <div className="page-body">
        {view === "calendar" ? (
          <>
            <div className="calendar" role="grid" aria-label={fmt.month(month)}>
              {weekdays.map((w) => (
                <div className="cal-head" key={w} role="columnheader">
                  {w}
                </div>
              ))}
              {grid.map((d) => {
                const list = byDay.get(dayKey(d)) ?? [];
                const past = startOfDay(d) < startOfDay(today);
                const cls = ["cal-day", d.getMonth() !== month.getMonth() ? "other" : "", sameDay(d, today) ? "today" : "", past ? "past" : ""].join(" ");
                return (
                  <div
                    key={dayKey(d)}
                    className={cls}
                    role="gridcell"
                    onClick={(e) => {
                      if (e.target === e.currentTarget) newOnDay(d);
                    }}
                    title={past ? undefined : "Click an empty spot to schedule a post on this day"}
                  >
                    <span className="cal-num" onClick={() => newOnDay(d)}>
                      {d.getDate()}
                    </span>
                    {list.slice(0, 3).map((p) => (
                      <button key={p.id} className={"chip " + p.status} onClick={() => setOpen(p)} title={`${STATUS_LABEL[p.status]}: ${preview(p.body, 200)}`}>
                        <span className="t">{fmt.time(postTime(p))}</span>
                        <span className="row" style={{ gap: 2 }}>
                          {Array.from(new Set(p.targets.map((t) => t.platform))).slice(0, 3).map((pl) => (
                            <PlatformIcon key={pl} platform={pl} size={12} />
                          ))}
                        </span>
                        <span className="x">{preview(p.options.title || p.body, 60) || "(no text)"}</span>
                      </button>
                    ))}
                    {list.length > 3 && (
                      <button className="more-link" onClick={() => setDayList(d)}>
                        +{list.length - 3} more
                      </button>
                    )}
                  </div>
                );
              })}
            </div>
            <p className="small muted" style={{ marginTop: 12 }}>
              Click a day to schedule a post on it. Drafts without a date are under Lists → Drafts.
            </p>
          </>
        ) : (
          <>
            <div className="segmented" style={{ marginBottom: 16 }} role="tablist">
              {(
                [
                  ["scheduled", "Scheduled"],
                  ["drafts", "Drafts"],
                  ["sent", "Sent"],
                  ["problems", "Needs attention"],
                ] as [ListTab, string][]
              ).map(([t, label]) => (
                <button key={t} className={tab === t ? "active" : ""} role="tab" aria-selected={tab === t} onClick={() => setTab(t)}>
                  {label} {counts[t] > 0 && <span className="small muted">({counts[t]})</span>}
                </button>
              ))}
            </div>
            {posts.length === 0 && !loading ? (
              <EmptyState icon={<CalendarDays size={28} />} title={tab === "scheduled" ? "Nothing scheduled" : tab === "drafts" ? "No drafts" : tab === "sent" ? "Nothing sent yet" : "All clear"}>
                <p>
                  {tab === "problems"
                    ? "Posts that failed or were missed show up here so you can retry them."
                    : "Posts you write in the composer appear here."}
                </p>
              </EmptyState>
            ) : (
              <div className="post-list">
                {posts.map((p) => (
                  <PostRow key={p.id} post={p} onOpen={() => setOpen(p)} />
                ))}
              </div>
            )}
          </>
        )}
      </div>
      {open && <PostDetail post={open} onClose={() => setOpen(null)} />}
      {dayList && (
        <Modal title={fmt.day(dayList)} onClose={() => setDayList(null)}>
          <div className="modal-body post-list">
            {(byDay.get(dayKey(dayList)) ?? []).map((p) => (
              <PostRow
                key={p.id}
                post={p}
                onOpen={() => {
                  setDayList(null);
                  setOpen(p);
                }}
              />
            ))}
          </div>
        </Modal>
      )}
    </>
  );
}
