// App frame: sidebar navigation, the current page, and the global dialogs.

import { useEffect } from "react";
import { BarChart3, CalendarDays, Columns3, Inbox, PenSquare, Settings as SettingsIcon, Users } from "lucide-react";
import { useApp, type Page } from "./lib/store";
import { api } from "./lib/api";
import { isUnread } from "./pages/InboxPage";
import { AccountAvatar, Spinner, Toasts } from "./components/common";
import { Composer } from "./components/Composer";
import { ConnectDialog } from "./components/ConnectDialog";
import { StreamsPage } from "./pages/StreamsPage";
import { PlannerPage } from "./pages/PlannerPage";
import { InboxPage } from "./pages/InboxPage";
import { AnalyticsPage } from "./pages/AnalyticsPage";
import { AccountsPage } from "./pages/AccountsPage";
import { SettingsPage } from "./pages/SettingsPage";

const NAV: { page: Page; label: string; icon: typeof Inbox }[] = [
  { page: "streams", label: "Streams", icon: Columns3 },
  { page: "planner", label: "Planner", icon: CalendarDays },
  { page: "inbox", label: "Inbox", icon: Inbox },
  { page: "analytics", label: "Analytics", icon: BarChart3 },
  { page: "accounts", label: "Accounts", icon: Users },
  { page: "settings", label: "Settings", icon: SettingsIcon },
];

function Sidebar() {
  const page = useApp((s) => s.page);
  const setPage = useApp((s) => s.setPage);
  const openComposer = useApp((s) => s.openComposer);
  const accounts = useApp((s) => s.accounts);
  const unread = useApp((s) => s.unread);
  return (
    <nav className="sidebar" aria-label="Main">
      <div className="brand">
        <span className="brand-mark" aria-hidden="true">
          <Columns3 size={17} />
        </span>
        Social Media Connector
      </div>
      <button className="btn primary compose-btn" onClick={() => openComposer()} title="New post (Ctrl+N)">
        <PenSquare size={16} /> New post
      </button>
      {NAV.map(({ page: p, label, icon: Icon }) => (
        <button key={p} className={"nav-item" + (page === p ? " active" : "")} onClick={() => setPage(p)} aria-current={page === p ? "page" : undefined}>
          <Icon size={18} />
          {label}
          {p === "inbox" && unread > 0 && page !== "inbox" && <span className="badge">{unread > 99 ? "99+" : unread}</span>}
          {p === "accounts" && accounts.some((a) => a.status !== "ok") && <span className="badge" style={{ background: "var(--warning)" }}>!</span>}
        </button>
      ))}
      <div className="sidebar-footer">
        <span className="small muted">
          {accounts.length} account{accounts.length === 1 ? "" : "s"} connected
        </span>
        <div className="sidebar-accounts">
          {accounts.slice(0, 12).map((a) => (
            <button key={a.id} className="icon-btn" style={{ width: 30, height: 30, padding: 0, borderRadius: "50%" }} onClick={() => setPage("accounts")} title={`${a.displayName} (${a.handle})`}>
              <AccountAvatar account={a} size={28} />
            </button>
          ))}
        </div>
      </div>
    </nav>
  );
}

/** Polls notifications in the background to show the inbox badge. */
function useUnreadPoller() {
  const page = useApp((s) => s.page);
  const accounts = useApp((s) => s.accounts);
  const info = useApp((s) => s.info);
  const setUnread = useApp((s) => s.setUnread);
  const hasInbox = accounts.some((a) => a.status === "ok" && info?.platforms.find((p) => p.id === a.platform)?.features.inbox);
  useEffect(() => {
    if (!hasInbox || page === "inbox") return;
    let stopped = false;
    const check = async () => {
      if (document.visibilityState !== "visible") return;
      try {
        const d = await api.loadInbox(null, null);
        if (!stopped) setUnread(d.items.filter((i) => isUnread(i, d.seen)).length);
      } catch {
        /* the inbox page shows errors */
      }
    };
    const first = setTimeout(() => void check(), 4000);
    const t = setInterval(() => void check(), 5 * 60_000);
    return () => {
      stopped = true;
      clearTimeout(first);
      clearInterval(t);
    };
  }, [hasInbox, page, setUnread]);
}

export default function App() {
  const ready = useApp((s) => s.ready);
  const loadError = useApp((s) => s.loadError);
  const page = useApp((s) => s.page);
  const composer = useApp((s) => s.composer);
  const connectOpen = useApp((s) => s.connectOpen);
  const init = useApp((s) => s.init);
  const openComposer = useApp((s) => s.openComposer);

  useEffect(() => {
    void init();
  }, [init]);

  useUnreadPoller();

  // Links inside posts open in the browser, never inside the app window.
  useEffect(() => {
    const onClick = (e: MouseEvent) => {
      const a = (e.target as HTMLElement | null)?.closest?.("a[href]") as HTMLAnchorElement | null;
      if (!a) return;
      const href = a.getAttribute("href") ?? "";
      if (href === "#" || href.startsWith("#")) return;
      e.preventDefault();
      if (/^https?:\/\//i.test(a.href)) void api.openUrl(a.href);
    };
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "n" && !useApp.getState().composer && !useApp.getState().connectOpen) {
        e.preventDefault();
        openComposer();
      }
    };
    document.addEventListener("click", onClick);
    document.addEventListener("auxclick", onClick);
    window.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("click", onClick);
      document.removeEventListener("auxclick", onClick);
      window.removeEventListener("keydown", onKey);
    };
  }, [openComposer]);

  if (!ready) {
    return (
      <div className="empty" style={{ height: "100vh" }}>
        <Spinner big />
      </div>
    );
  }
  if (loadError) {
    return (
      <div className="empty" style={{ height: "100vh" }}>
        <h2>Social Media Connector could not start</h2>
        <p>{loadError}</p>
      </div>
    );
  }

  return (
    <div className="app">
      <Sidebar />
      <main className="main">
        {page === "streams" && <StreamsPage />}
        {page === "planner" && <PlannerPage />}
        {page === "inbox" && <InboxPage />}
        {page === "analytics" && <AnalyticsPage />}
        {page === "accounts" && <AccountsPage />}
        {page === "settings" && <SettingsPage />}
      </main>
      {composer && <Composer seed={composer} />}
      {connectOpen && <ConnectDialog preset={connectOpen === true ? null : connectOpen} />}
      <Toasts />
    </div>
  );
}
