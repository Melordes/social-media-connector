// Connected accounts: status, follower counts, reconnect and remove.

import { useState } from "react";
import { ExternalLink, Lock, MoreHorizontal, Plus, RefreshCw, Rows3, Trash2, Users } from "lucide-react";
import type { Account } from "../lib/types";
import { api, errorMessage } from "../lib/api";
import { useApp, usePlatform } from "../lib/store";
import { formatCount } from "../lib/text";
import { AccountAvatar, Confirm, EmptyState, MenuButton, Spinner } from "../components/common";
import { AddStreamDialog } from "./StreamsPage";

function AccountCard({ account }: { account: Account }) {
  const platform = usePlatform(account.platform);
  const openConnect = useApp((s) => s.openConnect);
  const reloadAccounts = useApp((s) => s.reloadAccounts);
  const reloadStreams = useApp((s) => s.reloadStreams);
  const toast = useApp((s) => s.toast);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [addStream, setAddStream] = useState(false);

  const refresh = async () => {
    setBusy(true);
    try {
      await api.refreshAccount(account.id);
      await reloadAccounts();
      toast(`${account.displayName} is up to date.`, "success");
    } catch (e) {
      toast(errorMessage(e), "error");
      await reloadAccounts();
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setConfirm(false);
    try {
      await api.removeAccount(account.id);
      await Promise.all([reloadAccounts(), reloadStreams()]);
      toast(`${account.displayName} was removed.`, "success");
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };

  const stats = account.stats;
  const hasStreams = (platform?.streams.length ?? 0) > 0;

  return (
    <div className="card account-card">
      <div className="who">
        <AccountAvatar account={account} size={48} />
        <div className="grow">
          <strong>{account.displayName}</strong>
          <span>{account.handle}</span>
          <span>{platform?.name}</span>
        </div>
        {busy ? (
          <Spinner />
        ) : (
          <MenuButton
            label="Account options"
            items={[
              { label: "Refresh profile", icon: <RefreshCw size={15} />, onClick: () => void refresh() },
              ...(account.profileUrl ? [{ label: "Open profile", icon: <ExternalLink size={15} />, onClick: () => void api.openUrl(account.profileUrl!) }] : []),
              ...(hasStreams ? [{ label: "Add a stream…", icon: <Rows3 size={15} />, onClick: () => setAddStream(true) }] : []),
              { label: "Reconnect…", icon: <Lock size={15} />, onClick: () => openConnect(account.platform) },
              { label: "Remove account", icon: <Trash2 size={15} />, danger: true, separatorBefore: true, onClick: () => setConfirm(true) },
            ]}
          >
            <MoreHorizontal size={18} />
          </MenuButton>
        )}
      </div>
      {account.status !== "ok" ? (
        <div className="warn-box">
          <strong>Needs reconnecting.</strong> {account.statusMessage ?? "The saved sign-in no longer works."}
          <div style={{ marginTop: 8 }}>
            <button className="btn small" onClick={() => openConnect(account.platform)}>
              Reconnect
            </button>
          </div>
        </div>
      ) : stats && (stats.followers != null || stats.posts != null) ? (
        <div className="stat-row">
          {stats.followers != null && (
            <div>
              <strong>{formatCount(stats.followers)}</strong>
              <span>{account.platform === "telegram" ? "subscribers" : "followers"}</span>
            </div>
          )}
          {stats.following != null && (
            <div>
              <strong>{formatCount(stats.following)}</strong>
              <span>following</span>
            </div>
          )}
          {stats.posts != null && (
            <div>
              <strong>{formatCount(stats.posts)}</strong>
              <span>posts</span>
            </div>
          )}
        </div>
      ) : (
        <div className="small muted">{hasStreams ? "Connected." : "Connected for posting."}</div>
      )}
      {confirm && (
        <Confirm
          title={`Remove ${account.displayName}?`}
          message={
            <>
              The app forgets this account and its saved sign-in, and removes its streams. Scheduled posts will no longer go to it. Nothing is
              deleted on {platform?.name}. You can also revoke the app's access in {platform?.name}'s settings.
            </>
          }
          confirmLabel="Remove"
          danger
          onConfirm={() => void remove()}
          onCancel={() => setConfirm(false)}
        />
      )}
      {addStream && <AddStreamDialog presetAccount={account.id} onClose={() => setAddStream(false)} />}
    </div>
  );
}

export function AccountsPage() {
  const accounts = useApp((s) => s.accounts);
  const info = useApp((s) => s.info);
  const openConnect = useApp((s) => s.openConnect);
  const needs = accounts.filter((a) => a.status !== "ok").length;
  return (
    <>
      <header className="page-header">
        <h1>Accounts</h1>
        {accounts.length > 0 && (
          <span className="pill">
            {accounts.length} connected{needs ? ` · ${needs} need${needs === 1 ? "s" : ""} attention` : ""}
          </span>
        )}
        <span className="spacer" />
        <button className="btn primary" onClick={() => openConnect()}>
          <Plus size={15} /> Connect account
        </button>
      </header>
      <div className="page-body">
        {accounts.length === 0 ? (
          <EmptyState icon={<Users size={28} />} title="No accounts yet">
            <p>Connect the accounts you want to manage. Mastodon, Bluesky, Misskey, Telegram and Discord work right away; X, Facebook, LinkedIn, Reddit and Tumblr need a free developer app of your own.</p>
            <button className="btn primary" onClick={() => openConnect()}>
              <Plus size={15} /> Connect an account
            </button>
          </EmptyState>
        ) : (
          <div className="account-grid">
            {accounts.map((a) => (
              <AccountCard key={a.id} account={a} />
            ))}
          </div>
        )}
        {info && (
          <p className="small muted" style={{ marginTop: 24, display: "flex", gap: 8, alignItems: "center" }}>
            <Lock size={14} />
            {info.secretBackend === "keyring"
              ? `Sign-in tokens are kept in ${info.secretStoreName}.`
              : "No system keyring was found, so sign-in tokens are kept in a file only your user can read, in the app's data folder."}
          </p>
        )}
      </div>
    </>
  );
}
