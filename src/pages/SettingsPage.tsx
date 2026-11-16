// Settings: behaviour, developer apps for networks that need them, advanced
// options, and the About section (license, source code, open-source notices).

import { useEffect, useState } from "react";
import { ChevronRight, ExternalLink, ScrollText } from "lucide-react";
import type { Settings } from "../lib/types";
import { api, errorMessage } from "../lib/api";
import { useApp } from "../lib/store";
import { loadNotices, REPOSITORY_URL } from "../lib/about";
import { Modal, PlatformIcon, Spinner, Switch } from "../components/common";
import { DevAppForm } from "../components/ConnectDialog";

function Setting({ title, desc, children }: { title: string; desc?: string; children: React.ReactNode }) {
  return (
    <div className="setting">
      <div className="text">
        <strong>{title}</strong>
        {desc && <span>{desc}</span>}
      </div>
      {children}
    </div>
  );
}

export function SettingsPage() {
  const settings = useApp((s) => s.settings);
  const info = useApp((s) => s.info);
  const update = useApp((s) => s.updateSettings);
  const reloadInfo = useApp((s) => s.reloadInfo);
  const toast = useApp((s) => s.toast);
  const [autostart, setAutostart] = useState<boolean | null>(null);
  const [tray, setTray] = useState(true);
  const [port, setPort] = useState(String(settings?.oauthPort ?? 38917));
  const [showNotices, setShowNotices] = useState(false);

  useEffect(() => {
    api.getAutostart().then(setAutostart).catch(() => setAutostart(null));
    api.trayAvailable().then(setTray).catch(() => setTray(false));
  }, []);
  useEffect(() => setPort(String(settings?.oauthPort ?? 38917)), [settings?.oauthPort]);

  if (!settings || !info) return null;
  const set = (patch: Partial<Settings>) => void update(patch);
  const devPlatforms = info.platforms.filter((p) => p.connect.needsApp);

  return (
    <>
      <header className="page-header">
        <h1>Settings</h1>
      </header>
      <div className="page-body">
        <div className="settings">
          <section className="card card-pad">
            <h2 className="section-title">General</h2>
            <Setting title="Appearance">
              <div className="segmented" role="radiogroup" aria-label="Theme">
                {(["system", "light", "dark"] as const).map((t) => (
                  <button key={t} className={settings.theme === t ? "active" : ""} role="radio" aria-checked={settings.theme === t} onClick={() => set({ theme: t })}>
                    {t === "system" ? "System" : t === "light" ? "Light" : "Dark"}
                  </button>
                ))}
              </div>
            </Setting>
            <Setting
              title="Keep running in the tray when the window is closed"
              desc={tray ? "Scheduled posts only go out while the app runs." : "No system tray was found, so closing the window quits the app."}
            >
              <Switch checked={settings.closeToTray} onChange={(v) => set({ closeToTray: v })} label="Keep running in the tray" />
            </Setting>
            <Setting title="Start when I log in" desc="Starts quietly in the tray, so scheduled posts go out after a restart.">
              {autostart === null ? (
                <span className="small muted">Not available</span>
              ) : (
                <Switch
                  checked={autostart}
                  onChange={(v) => {
                    api
                      .setAutostart(v)
                      .then(setAutostart)
                      .catch((e) => toast(errorMessage(e), "error"));
                  }}
                  label="Start when I log in"
                />
              )}
            </Setting>
            <Setting title="Notify me when a post is published">
              <Switch checked={settings.notifyPublished} onChange={(v) => set({ notifyPublished: v })} label="Notify when published" />
            </Setting>
            <Setting title="Notify me when a post fails or is missed">
              <Switch checked={settings.notifyFailed} onChange={(v) => set({ notifyFailed: v })} label="Notify on failures" />
            </Setting>
            <Setting
              title="Overdue scheduled posts"
              desc="If the app was not running at the scheduled time, how late may a post still go out by itself? Later ones wait in the Planner as missed."
            >
              <select className="select" style={{ width: 190 }} value={settings.lateGraceMinutes} onChange={(e) => set({ lateGraceMinutes: Number(e.target.value) })}>
                <option value={0}>Never send late</option>
                <option value={15}>Up to 15 minutes late</option>
                <option value={60}>Up to 1 hour late</option>
                <option value={360}>Up to 6 hours late</option>
                <option value={1440}>Up to 1 day late</option>
                <option value={-1}>Always send</option>
              </select>
            </Setting>
            <Setting title="First day of the week" desc="For the Planner calendar.">
              <select className="select" style={{ width: 190 }} value={settings.weekStartsMonday ? "1" : "0"} onChange={(e) => set({ weekStartsMonday: e.target.value === "1" })}>
                <option value="1">Monday</option>
                <option value="0">Sunday</option>
              </select>
            </Setting>
          </section>

          <section className="card card-pad">
            <h2 className="section-title">Developer apps</h2>
            <p className="section-sub">
              X, Facebook, LinkedIn, Reddit and Tumblr only accept apps registered by their users. Register an app with each network you want to
              use and paste its keys here (X charges per request; the others are free for personal use). Mastodon, Bluesky, Misskey, Telegram and
              Discord need nothing here.
            </p>
            {devPlatforms.map((p) => {
              const saved = info.appCredentials.find((c) => c.platform === p.id);
              return (
                <details className="devapp" key={p.id}>
                  <summary>
                    <PlatformIcon platform={p.id} size={24} />
                    <strong className="grow">{p.name}</strong>
                    <span className={"pill " + (saved ? "success" : "")}>{saved ? "Saved" : "Not set up"}</span>
                    <ChevronRight size={16} className="muted" />
                  </summary>
                  <div className="devapp-body">
                    <DevAppForm platform={p} compact />
                  </div>
                </details>
              );
            })}
          </section>

          <section className="card card-pad">
            <h2 className="section-title">Advanced</h2>
            <Setting
              title="Sign-in callback port"
              desc="Browser sign-ins come back to this app on http://127.0.0.1 or http://localhost at this port. If you change it, update the redirect URI in your developer apps."
            >
              <div className="row">
                <input className="input" style={{ width: 100 }} value={port} inputMode="numeric" onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
                <button
                  className="btn"
                  disabled={Number(port) === settings.oauthPort}
                  onClick={async () => {
                    const n = Number(port);
                    if (!(n >= 1024 && n <= 65535)) {
                      toast("Choose a port between 1024 and 65535.", "error");
                      return;
                    }
                    await update({ oauthPort: n });
                    await reloadInfo();
                  }}
                >
                  Apply
                </button>
              </div>
            </Setting>
            <Setting title="New streams refresh every">
              <select className="select" style={{ width: 190 }} value={settings.defaultRefreshMinutes} onChange={(e) => set({ defaultRefreshMinutes: Number(e.target.value) })}>
                {[0, 2, 5, 10, 15, 30].map((m) => (
                  <option key={m} value={m}>
                    {m === 0 ? "Never (manual)" : `${m} minutes`}
                  </option>
                ))}
              </select>
            </Setting>
            <Setting title="Data folder" desc={info.dataDir}>
              <span />
            </Setting>
            <Setting
              title="Where sign-ins are stored"
              desc={
                info.secretBackend === "keyring"
                  ? info.secretStoreName.charAt(0).toUpperCase() + info.secretStoreName.slice(1)
                  : "A file in the data folder that only your user can read"
              }
            >
              <span />
            </Setting>
            <Setting title="Quit" desc="Closes the app completely, tray icon included. Scheduled posts wait until it runs again.">
              <button className="btn" onClick={() => void api.quit()}>
                Quit app
              </button>
            </Setting>
          </section>

          <section className="card card-pad">
            <h2 className="section-title">About</h2>
            <Setting
              title={`Social Media Connector ${info.version}`}
              desc="Free and unencumbered software in the public domain (The Unlicense): anyone may use, change and share it."
            >
              <button className="btn" onClick={() => void api.openUrl(REPOSITORY_URL)}>
                <ExternalLink size={15} /> Source code
              </button>
            </Setting>
            <Setting title="Open-source licenses" desc="The notices and license texts of the libraries the app is built with.">
              <button className="btn" onClick={() => setShowNotices(true)}>
                <ScrollText size={15} /> View licenses
              </button>
            </Setting>
          </section>
        </div>
      </div>
      {showNotices && <NoticesDialog onClose={() => setShowNotices(false)} />}
    </>
  );
}

/** The third-party notices, generated at build time and loaded on demand. */
function NoticesDialog({ onClose }: { onClose: () => void }) {
  const [text, setText] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    loadNotices()
      .then(setText)
      .catch(() => setFailed(true));
  }, []);
  return (
    <Modal title="Open-source licenses" onClose={onClose} size="wide">
      <div className="modal-body">
        {failed ? (
          <p className="error-text">The license notices could not be loaded.</p>
        ) : text === null ? (
          <Spinner />
        ) : (
          <pre className="notices">{text}</pre>
        )}
      </div>
    </Modal>
  );
}
