// Inbox: notifications from every account in one list, newest first.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { CheckCheck, Inbox, RefreshCw } from "lucide-react";
import type { FeedItem, InboxPage as InboxData, NotificationKind } from "../lib/types";
import { api, errorMessage } from "../lib/api";
import { useApp } from "../lib/store";
import { EmptyState, Spinner } from "../components/common";
import { FeedItemCard } from "../components/FeedItemCard";

type Filter = "all" | "conversation" | "likes" | "reposts" | "follows" | "other";

const FILTERS: [Filter, string][] = [
  ["all", "All"],
  ["conversation", "Mentions and replies"],
  ["likes", "Likes"],
  ["reposts", "Reposts"],
  ["follows", "Follows"],
  ["other", "Other"],
];

function matches(kind: NotificationKind | undefined, f: Filter): boolean {
  if (f === "all") return true;
  if (!kind) return f === "other";
  switch (f) {
    case "conversation":
      return kind === "mention" || kind === "reply" || kind === "quote" || kind === "message";
    case "likes":
      return kind === "like";
    case "reposts":
      return kind === "repost";
    case "follows":
      return kind === "follow" || kind === "followRequest";
    default:
      return ["poll", "update", "other"].includes(kind);
  }
}

export function isUnread(item: FeedItem, seen: Record<string, string>): boolean {
  const s = seen[item.accountId];
  if (!s) return true;
  return Date.parse(item.createdAt) > Date.parse(s);
}

export function InboxPage() {
  const accounts = useApp((s) => s.accounts);
  const info = useApp((s) => s.info);
  const toast = useApp((s) => s.toast);
  const setUnread = useApp((s) => s.setUnread);
  const inboxAccounts = accounts.filter((a) => info?.platforms.find((p) => p.id === a.platform)?.features.inbox);
  const [data, setData] = useState<InboxData | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [filter, setFilter] = useState<Filter>("all");
  const [accountFilter, setAccountFilter] = useState<string>("all");
  const dataRef = useRef<InboxData | null>(null);
  dataRef.current = data;

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const d = await api.loadInbox(null, null);
      setData(d);
      setUnread(d.items.filter((i) => isUnread(i, d.seen)).length);
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setLoading(false);
    }
  }, [setUnread, toast]);

  useEffect(() => {
    void load();
  }, [load, accounts.length]);

  // Leaving the inbox marks everything that was shown as read.
  useEffect(() => {
    return () => {
      if (dataRef.current && dataRef.current.items.length) {
        void api.markInboxSeen(null).then(() => setUnread(0));
      }
    };
  }, [setUnread]);

  const loadMore = async () => {
    if (!data || !Object.keys(data.cursors).length) return;
    setLoadingMore(true);
    try {
      const more = await api.loadInbox(null, data.cursors);
      setData((d) => {
        if (!d) return more;
        const seenIds = new Set(d.items.map((i) => `${i.accountId}:${i.id}`));
        const items = [...d.items, ...more.items.filter((i) => !seenIds.has(`${i.accountId}:${i.id}`))];
        items.sort((a, b) => b.createdAt.localeCompare(a.createdAt));
        return { ...d, items, cursors: more.cursors, errors: more.errors };
      });
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setLoadingMore(false);
    }
  };

  const markAll = async () => {
    try {
      await api.markInboxSeen(null);
      const now = new Date().toISOString();
      setData((d) => (d ? { ...d, seen: Object.fromEntries(inboxAccounts.map((a) => [a.id, now])) } : d));
      setUnread(0);
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };

  const shown = useMemo(
    () =>
      (data?.items ?? []).filter(
        (i) => matches(i.notification?.kind, filter) && (accountFilter === "all" || i.accountId === accountFilter),
      ),
    [data, filter, accountFilter],
  );
  const unreadCount = data ? data.items.filter((i) => isUnread(i, data.seen)).length : 0;

  return (
    <>
      <header className="page-header">
        <h1>Inbox</h1>
        {unreadCount > 0 && <span className="pill accent">{unreadCount} new</span>}
        <span className="spacer" />
        <button className="btn" onClick={() => void load()} disabled={loading}>
          {loading ? <Spinner /> : <RefreshCw size={15} />} Refresh
        </button>
        <button className="btn" onClick={() => void markAll()} disabled={!unreadCount}>
          <CheckCheck size={15} /> Mark all as read
        </button>
      </header>
      <div className="page-body">
        <div className="inbox">
          {inboxAccounts.length === 0 ? (
            <EmptyState icon={<Inbox size={28} />} title="No inbox accounts">
              <p>Notifications from Mastodon, Bluesky, Misskey, Reddit and Tumblr accounts collect here. Connect one to see its mentions, replies, likes and follows.</p>
            </EmptyState>
          ) : (
            <>
              <div className="filters">
                <div className="segmented" role="tablist" aria-label="Notification type">
                  {FILTERS.map(([f, label]) => (
                    <button key={f} className={filter === f ? "active" : ""} role="tab" aria-selected={filter === f} onClick={() => setFilter(f)}>
                      {label}
                    </button>
                  ))}
                </div>
                <select className="select" style={{ width: "auto" }} value={accountFilter} onChange={(e) => setAccountFilter(e.target.value)} aria-label="Account">
                  <option value="all">All accounts</option>
                  {inboxAccounts.map((a) => (
                    <option key={a.id} value={a.id}>
                      {a.displayName} ({a.handle})
                    </option>
                  ))}
                </select>
              </div>
              {data?.errors.map((e) => {
                const a = accounts.find((x) => x.id === e.accountId);
                return (
                  <div className="error-box" key={e.accountId} style={{ marginBottom: 10 }}>
                    {a ? `${a.displayName} (${a.handle})` : "An account"}: {e.message}
                  </div>
                );
              })}
              {loading && !data ? (
                <div className="empty">
                  <Spinner big />
                </div>
              ) : shown.length === 0 ? (
                <EmptyState icon={<Inbox size={28} />} title="Nothing here">
                  <p>No notifications match this filter.</p>
                </EmptyState>
              ) : (
                <div className="card" style={{ opacity: loading ? 0.6 : 1 }}>
                  {shown.map((item) => {
                    const account = accounts.find((a) => a.id === item.accountId);
                    if (!account) return null;
                    return (
                      <FeedItemCard
                        key={`${item.accountId}:${item.id}`}
                        item={item}
                        account={account}
                        showAccount
                        unread={data ? isUnread(item, data.seen) : false}
                        onChange={(next) =>
                          setData((d) =>
                            d ? { ...d, items: d.items.map((i) => (i.id === item.id && i.accountId === item.accountId ? next : i)) } : d,
                          )
                        }
                      />
                    );
                  })}
                </div>
              )}
              {data && Object.keys(data.cursors).length > 0 && shown.length > 0 && (
                <div style={{ textAlign: "center", marginTop: 14 }}>
                  <button className="btn" onClick={() => void loadMore()} disabled={loadingMore}>
                    {loadingMore ? <Spinner /> : null} Load older
                  </button>
                </div>
              )}
            </>
          )}
        </div>
      </div>
    </>
  );
}
