// Streams: one column per feed (home timeline, notifications, a hashtag...),
// like Hootsuite's boards.

import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowLeft, ArrowRight, Columns3, MoreHorizontal, Plus, RefreshCw, Settings2, Trash2, Link2 } from "lucide-react";
import type { Account, FeedItem, Stream } from "../lib/types";
import { api, errorKind, errorMessage } from "../lib/api";
import { useApp, usePlatform } from "../lib/store";
import { AccountAvatar, EmptyState, MenuButton, Modal, Spinner } from "../components/common";
import { FeedItemCard } from "../components/FeedItemCard";

const REFRESH_CHOICES = [0, 1, 2, 5, 10, 15, 30, 60];

function streamLabel(stream: Stream, kindLabel: string | undefined): string {
  if (stream.title) return stream.title;
  const base = kindLabel ?? stream.kind;
  if (!stream.param) return base;
  return stream.kind === "hashtag" || stream.kind === "tagged" ? `#${stream.param.replace(/^#/, "")}` : `${base}: ${stream.param}`;
}

function StreamColumn({
  stream,
  account,
  refreshSignal,
  index,
  total,
  onMove,
}: {
  stream: Stream;
  account: Account;
  refreshSignal: number;
  index: number;
  total: number;
  onMove: (from: number, to: number) => void;
}) {
  const platform = usePlatform(account.platform);
  const reloadStreams = useApp((s) => s.reloadStreams);
  const setPage = useApp((s) => s.setPage);
  const toast = useApp((s) => s.toast);
  const kind = platform?.streams.find((k) => k.id === stream.kind);
  const [items, setItems] = useState<FeedItem[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<{ message: string; kind: string | null } | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const body = useRef<HTMLDivElement>(null);
  const loadingRef = useRef(false);

  const load = useCallback(async () => {
    if (loadingRef.current) return;
    loadingRef.current = true;
    setLoading(true);
    try {
      const page = await api.loadStream(stream.id, null);
      setItems(page.items);
      setCursor(page.cursor);
      setError(null);
    } catch (e) {
      setError({ message: errorMessage(e), kind: errorKind(e) });
    } finally {
      loadingRef.current = false;
      setLoading(false);
    }
  }, [stream.id]);

  const loadMore = useCallback(async () => {
    if (!cursor || loadingMore || loadingRef.current) return;
    setLoadingMore(true);
    try {
      const page = await api.loadStream(stream.id, cursor);
      setItems((prev) => {
        const seen = new Set(prev.map((i) => i.id));
        return [...prev, ...page.items.filter((i) => !seen.has(i.id))];
      });
      setCursor(page.items.length ? page.cursor : null);
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setLoadingMore(false);
    }
  }, [cursor, loadingMore, stream.id, toast]);

  useEffect(() => {
    void load();
  }, [load, refreshSignal, account.status]);

  // Show the user's own new posts soon after they are published.
  const postsVersion = useApp((s) => s.postsVersion);
  const firstVersion = useRef(postsVersion);
  useEffect(() => {
    if (postsVersion === firstVersion.current || (stream.kind !== "home" && stream.kind !== "me")) return;
    const t = setTimeout(() => void load(), 1500);
    return () => clearTimeout(t);
  }, [postsVersion, stream.kind, load]);

  useEffect(() => {
    if (!stream.refreshMinutes) return;
    const t = setInterval(() => {
      if (document.visibilityState === "visible") void load();
    }, stream.refreshMinutes * 60_000);
    return () => clearInterval(t);
  }, [load, stream.refreshMinutes]);

  const onScroll = () => {
    const el = body.current;
    if (el && el.scrollTop + el.clientHeight > el.scrollHeight - 400) void loadMore();
  };

  const remove = async () => {
    try {
      await api.removeStream(stream.id);
      await reloadStreams();
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };

  const title = streamLabel(stream, kind?.label);

  return (
    <section className="column" aria-label={`${title} — ${account.handle}`}>
      <header className="column-header">
        <AccountAvatar account={account} size={32} />
        <div className="column-title">
          <strong>{title}</strong>
          <span>
            {account.displayName} · {account.handle}
          </span>
        </div>
        <button className="icon-btn" onClick={() => void load()} disabled={loading} aria-label="Refresh" title="Refresh">
          {loading ? <Spinner /> : <RefreshCw size={16} />}
        </button>
        <MenuButton
          label="Stream options"
          items={[
            { label: "Stream settings…", icon: <Settings2 size={15} />, onClick: () => setSettingsOpen(true) },
            { label: "Move left", icon: <ArrowLeft size={15} />, disabled: index === 0, onClick: () => onMove(index, index - 1) },
            { label: "Move right", icon: <ArrowRight size={15} />, disabled: index === total - 1, onClick: () => onMove(index, index + 1) },
            ...(account.profileUrl
              ? [{ label: "Open profile in browser", icon: <Link2 size={15} />, onClick: () => void api.openUrl(account.profileUrl!) }]
              : []),
            { label: "Remove stream", icon: <Trash2 size={15} />, danger: true, separatorBefore: true, onClick: () => void remove() },
          ]}
        >
          <MoreHorizontal size={16} />
        </MenuButton>
      </header>
      <div className="column-body" ref={body} onScroll={onScroll}>
        {error && (
          <div className="column-error">
            {error.message}
            <div className="row">
              {error.kind === "unauthorized" ? (
                <button className="btn small" onClick={() => setPage("accounts")}>
                  Go to Accounts
                </button>
              ) : (
                <button className="btn small" onClick={() => void load()}>
                  Try again
                </button>
              )}
            </div>
          </div>
        )}
        {!error && loading && items.length === 0 && <div className="column-status">Loading…</div>}
        {!error && !loading && items.length === 0 && <div className="column-status">Nothing here yet.</div>}
        {items.map((item) => (
          <FeedItemCard
            key={item.id}
            item={item}
            account={account}
            onChange={(next) => setItems((prev) => prev.map((i) => (i.id === item.id ? next : i)))}
            onRemove={(gone) => setItems((prev) => prev.filter((i) => i.id !== gone.id))}
          />
        ))}
        {cursor && items.length > 0 && (
          <div className="column-status">
            {loadingMore ? (
              <Spinner />
            ) : (
              <button className="btn small" onClick={() => void loadMore()}>
                Load more
              </button>
            )}
          </div>
        )}
      </div>
      {settingsOpen && <StreamSettings stream={stream} defaultTitle={title} onClose={() => setSettingsOpen(false)} />}
    </section>
  );
}

function StreamSettings({ stream, defaultTitle, onClose }: { stream: Stream; defaultTitle: string; onClose: () => void }) {
  const reloadStreams = useApp((s) => s.reloadStreams);
  const toast = useApp((s) => s.toast);
  const [title, setTitle] = useState(stream.title ?? "");
  const [minutes, setMinutes] = useState(stream.refreshMinutes);
  const save = async () => {
    try {
      await api.updateStream(stream.id, title.trim() || null, minutes);
      await reloadStreams();
      onClose();
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };
  return (
    <Modal
      title="Stream settings"
      onClose={onClose}
      footer={
        <>
          <span className="spacer" />
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" onClick={() => void save()}>
            Save
          </button>
        </>
      }
    >
      <div className="modal-body">
        <div className="field">
          <label htmlFor="stream-title">Name</label>
          <input id="stream-title" className="input" value={title} placeholder={defaultTitle} onChange={(e) => setTitle(e.target.value)} />
        </div>
        <div className="field">
          <label htmlFor="stream-refresh">Refresh automatically</label>
          <select id="stream-refresh" className="select" value={minutes} onChange={(e) => setMinutes(Number(e.target.value))}>
            {REFRESH_CHOICES.map((m) => (
              <option key={m} value={m}>
                {m === 0 ? "Never" : m === 1 ? "Every minute" : `Every ${m} minutes`}
              </option>
            ))}
          </select>
          <span className="help">Some networks limit how often an app may read; X in particular allows only a few reads per 15 minutes on smaller plans.</span>
        </div>
      </div>
    </Modal>
  );
}

export function AddStreamDialog({ onClose, presetAccount }: { onClose: () => void; presetAccount?: string }) {
  const accounts = useApp((s) => s.accounts);
  const info = useApp((s) => s.info);
  const reloadStreams = useApp((s) => s.reloadStreams);
  const toast = useApp((s) => s.toast);
  const readable = accounts.filter((a) => (info?.platforms.find((p) => p.id === a.platform)?.streams.length ?? 0) > 0);
  const [accountId, setAccountId] = useState(presetAccount ?? readable[0]?.id ?? "");
  const account = accounts.find((a) => a.id === accountId);
  const platform = info?.platforms.find((p) => p.id === account?.platform);
  const [kind, setKind] = useState<string>("");
  const [param, setParam] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setKind(platform?.streams[0]?.id ?? "");
    setParam("");
  }, [accountId, platform]);
  const kindInfo = platform?.streams.find((k) => k.id === kind);
  const add = async () => {
    if (!account || !kind) return;
    setBusy(true);
    try {
      await api.addStream(account.id, kind, kindInfo?.param ? param : null);
      await reloadStreams();
      onClose();
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal
      title="Add a stream"
      onClose={onClose}
      footer={
        <>
          <span className="spacer" />
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={!account || !kind || busy || (!!kindInfo?.param && !param.trim())} onClick={() => void add()}>
            Add stream
          </button>
        </>
      }
    >
      <div className="modal-body">
        {readable.length === 0 ? (
          <p className="muted">None of your connected accounts has streams to read. LinkedIn, Telegram and Discord accounts are for posting only.</p>
        ) : (
          <>
            <div className="field">
              <label htmlFor="stream-account">Account</label>
              <select id="stream-account" className="select" value={accountId} onChange={(e) => setAccountId(e.target.value)}>
                {readable.map((a) => (
                  <option key={a.id} value={a.id}>
                    {info?.platforms.find((p) => p.id === a.platform)?.name}: {a.displayName} ({a.handle})
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <span className="field-label">Stream</span>
              <div role="radiogroup">
                {platform?.streams.map((k) => (
                  <label key={k.id} className="check">
                    <input type="radio" name="kind" checked={kind === k.id} onChange={() => setKind(k.id)} />
                    <span>{k.label}</span>
                  </label>
                ))}
              </div>
            </div>
            {kindInfo?.param && (
              <div className="field">
                <label htmlFor="stream-param">{kindInfo.param.label}</label>
                <input
                  id="stream-param"
                  className="input"
                  value={param}
                  placeholder={kindInfo.param.placeholder}
                  onChange={(e) => setParam(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && void add()}
                  autoFocus
                />
              </div>
            )}
          </>
        )}
      </div>
    </Modal>
  );
}

export function StreamsPage() {
  const accounts = useApp((s) => s.accounts);
  const streams = useApp((s) => s.streams);
  const reloadStreams = useApp((s) => s.reloadStreams);
  const openConnect = useApp((s) => s.openConnect);
  const toast = useApp((s) => s.toast);
  const [adding, setAdding] = useState(false);
  const [refreshSignal, setRefreshSignal] = useState(0);

  const visible = streams.filter((s) => accounts.some((a) => a.id === s.accountId));

  const move = async (from: number, to: number) => {
    const ids = visible.map((s) => s.id);
    const [id] = ids.splice(from, 1);
    ids.splice(to, 0, id);
    try {
      await api.reorderStreams(ids);
      await reloadStreams();
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };

  return (
    <>
      <header className="page-header">
        <h1>Streams</h1>
        <span className="spacer" />
        {visible.length > 0 && (
          <button className="btn" onClick={() => setRefreshSignal((n) => n + 1)}>
            <RefreshCw size={15} /> Refresh all
          </button>
        )}
        <button className="btn" onClick={() => setAdding(true)} disabled={accounts.length === 0}>
          <Plus size={15} /> Add stream
        </button>
      </header>
      {accounts.length === 0 ? (
        <div className="page-body">
          <EmptyState icon={<Columns3 size={28} />} title="Connect your first account">
            <p>
              Streams show your timelines, notifications and searches side by side. Connect Mastodon, Bluesky, X, Facebook Pages,
              LinkedIn and more to get started.
            </p>
            <button className="btn primary" onClick={() => openConnect()}>
              <Plus size={15} /> Connect an account
            </button>
          </EmptyState>
        </div>
      ) : visible.length === 0 ? (
        <div className="page-body">
          <EmptyState icon={<Columns3 size={28} />} title="No streams yet">
            <p>Add a stream to follow a timeline, your notifications or a hashtag.</p>
            <button className="btn primary" onClick={() => setAdding(true)}>
              <Plus size={15} /> Add stream
            </button>
          </EmptyState>
        </div>
      ) : (
        <div className="streams">
          {visible.map((s, i) => {
            const account = accounts.find((a) => a.id === s.accountId)!;
            return (
              <StreamColumn
                key={s.id}
                stream={s}
                account={account}
                refreshSignal={refreshSignal}
                index={i}
                total={visible.length}
                onMove={(a, b) => void move(a, b)}
              />
            );
          })}
          <button className="add-column" onClick={() => setAdding(true)}>
            <Plus size={22} />
            Add stream
          </button>
        </div>
      )}
      {adding && <AddStreamDialog onClose={() => setAdding(false)} />}
    </>
  );
}
