// Connecting an account: pick a network, set up the developer app if the
// network needs one, then sign in in the browser or fill in a short form.

import { useEffect, useRef, useState } from "react";
import { ArrowLeft, Check, Copy, ExternalLink, KeyRound } from "lucide-react";
import type { Account, Platform, PlatformInfo } from "../lib/types";
import { api, errorKind, errorMessage } from "../lib/api";
import { useApp } from "../lib/store";
import { AccountAvatar, Modal, PlatformIcon, Spinner } from "./common";

export function CopyField({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="copy-row">
      <input className="input mono" readOnly value={value} aria-label={label} onFocus={(e) => e.target.select()} />
      <button
        className="btn"
        onClick={() => {
          void navigator.clipboard.writeText(value).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          });
        }}
      >
        {copied ? <Check size={15} /> : <Copy size={15} />} {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}

/** Developer app credentials for networks that need the user's own app. */
export function DevAppForm({ platform, onSaved, compact }: { platform: PlatformInfo; onSaved?: () => void; compact?: boolean }) {
  const info = useApp((s) => s.info);
  const reloadInfo = useApp((s) => s.reloadInfo);
  const toast = useApp((s) => s.toast);
  const existing = info?.appCredentials.find((c) => c.platform === platform.id);
  const [clientId, setClientId] = useState(existing?.clientId ?? "");
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const secretField = platform.connect.appFields.find((f) => f.key === "clientSecret");
  const idField = platform.connect.appFields.find((f) => f.key === "clientId");
  const redirect = info?.redirectUris[platform.id] ?? "";
  const save = async () => {
    setBusy(true);
    try {
      await api.setAppCredentials(platform.id, clientId, secret ? secret : null);
      await reloadInfo();
      setSecret("");
      toast(`${platform.name} developer app saved.`, "success");
      onSaved?.();
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(false);
    }
  };
  const clear = async () => {
    setBusy(true);
    try {
      await api.clearAppCredentials(platform.id);
      await reloadInfo();
      setClientId("");
      setSecret("");
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(false);
    }
  };
  const needsSecret = !!secretField?.required && !existing?.hasSecret && !secret;
  return (
    <div>
      {!compact && <p className="muted" style={{ marginTop: 0 }}>{platform.name} only accepts apps that you register yourself with {platform.name}. It takes a few minutes; the steps are below.</p>}
      <ol className="steps">
        {platform.connect.setupSteps.map((s, i) => (
          <li key={i}>{s}</li>
        ))}
      </ol>
      {platform.connect.setupUrl && (
        <p>
          <button className="btn small" onClick={() => void api.openUrl(platform.connect.setupUrl!)}>
            <ExternalLink size={14} /> Open the {platform.name} developer site
          </button>
        </p>
      )}
      <div className="field">
        <span className="field-label">Redirect / callback URI to register</span>
        <CopyField value={redirect} label="Redirect URI" />
        <span className="help">It must match exactly, including the port. You can change the port in Settings → Advanced.</span>
      </div>
      <div className="field">
        <label htmlFor={`app-id-${platform.id}`}>{idField?.label ?? "Client ID"}</label>
        <input id={`app-id-${platform.id}`} className="input mono" value={clientId} onChange={(e) => setClientId(e.target.value)} autoComplete="off" spellCheck={false} />
        {idField?.help && <span className="help">{idField.help}</span>}
      </div>
      {secretField && (
        <div className="field">
          <label htmlFor={`app-secret-${platform.id}`}>
            {secretField.label}
            {!secretField.required && " (optional)"}
          </label>
          <input
            id={`app-secret-${platform.id}`}
            className="input mono"
            type="password"
            value={secret}
            placeholder={existing?.hasSecret ? "Saved — type to replace" : ""}
            onChange={(e) => setSecret(e.target.value)}
            autoComplete="off"
          />
          {secretField.help && <span className="help">{secretField.help}</span>}
        </div>
      )}
      <div className="row">
        <button className="btn primary" disabled={busy || !clientId.trim() || needsSecret} onClick={() => void save()}>
          <KeyRound size={15} /> Save developer app
        </button>
        {existing && (
          <button className="btn danger" disabled={busy} onClick={() => void clear()}>
            Remove
          </button>
        )}
      </div>
    </div>
  );
}

type Step =
  | { kind: "pick" }
  | { kind: "setup"; platform: PlatformInfo }
  | { kind: "form"; platform: PlatformInfo }
  | { kind: "waiting"; platform: PlatformInfo; authUrl: string }
  | { kind: "done"; platform: PlatformInfo; accounts: Account[] };

export function ConnectDialog({ preset }: { preset: Platform | null }) {
  const info = useApp((s) => s.info);
  const close = useApp((s) => s.closeConnect);
  const reloadAccounts = useApp((s) => s.reloadAccounts);
  const reloadStreams = useApp((s) => s.reloadStreams);
  const platforms = info?.platforms ?? [];
  const hasApp = (p: PlatformInfo) => !p.connect.needsApp || !!info?.appCredentials.some((c) => c.platform === p.id);
  const startFor = (p: PlatformInfo): Step => (hasApp(p) ? { kind: "form", platform: p } : { kind: "setup", platform: p });
  const [step, setStep] = useState<Step>(() => {
    const p = platforms.find((x) => x.id === preset);
    return p ? startFor(p) : { kind: "pick" };
  });
  const [fields, setFields] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const active = useRef(true);

  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);

  const choose = (p: PlatformInfo) => {
    setFields({});
    setError(null);
    setStep(startFor(p));
  };

  const finish = async (platform: PlatformInfo, accounts: Account[]) => {
    await Promise.all([reloadAccounts(), reloadStreams()]);
    if (active.current) setStep({ kind: "done", platform, accounts });
  };

  const connect = async (platform: PlatformInfo) => {
    setError(null);
    for (const f of platform.connect.fields) {
      if (f.required && !(fields[f.key] ?? "").trim()) {
        setError(`Enter ${f.label.toLowerCase()}.`);
        return;
      }
    }
    setBusy(true);
    try {
      if (platform.connect.method === "form") {
        const accounts = await api.connectForm(platform.id, fields);
        await finish(platform, accounts);
      } else {
        const start = await api.beginConnect(platform.id, fields);
        if (!active.current) return;
        setStep({ kind: "waiting", platform, authUrl: start.authUrl });
        try {
          const accounts = await api.completeConnect(start.flowId);
          await finish(platform, accounts);
        } catch (e) {
          if (!active.current) return;
          if (errorKind(e) !== "cancelled") setError(errorMessage(e));
          setStep({ kind: "form", platform });
        }
      }
    } catch (e) {
      if (active.current) setError(errorMessage(e));
    } finally {
      if (active.current) setBusy(false);
    }
  };

  const cancelAndClose = () => {
    if (step.kind === "waiting") void api.cancelConnect();
    close();
  };

  const header =
    step.kind === "pick" ? "Connect an account" : step.kind === "done" ? "Connected" : `Connect ${step.platform.name}`;
  const icon = step.kind !== "pick" ? <PlatformIcon platform={step.platform.id} size={26} /> : undefined;

  return (
    <Modal title={header} onClose={cancelAndClose} size="medium" icon={icon} closeOnOverlay={step.kind === "pick" || step.kind === "done"}>
      <div className="modal-body">
        {step.kind === "pick" && (
          <>
            <p className="muted" style={{ marginTop: 0 }}>
              Choose a network. You can connect as many accounts as you like, including several on the same network.
            </p>
            <div className="platform-grid">
              {platforms.map((p) => (
                <button key={p.id} className="platform-card" onClick={() => choose(p)}>
                  <PlatformIcon platform={p.id} size={34} />
                  <div>
                    <strong>{p.name}</strong>
                    <span>{p.summary}</span>
                    {p.connect.needsApp && (
                      <div style={{ marginTop: 6 }}>
                        <span className={"pill " + (hasApp(p) ? "success" : "warning")}>{hasApp(p) ? "Developer app saved" : "Needs developer app"}</span>
                      </div>
                    )}
                  </div>
                </button>
              ))}
            </div>
          </>
        )}

        {step.kind === "setup" && (
          <>
            <DevAppForm platform={step.platform} onSaved={() => setStep({ kind: "form", platform: step.platform })} />
          </>
        )}

        {step.kind === "form" && (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void connect(step.platform);
            }}
          >
            {step.platform.connect.help && <div className="info-box" style={{ marginBottom: 16 }}>{step.platform.connect.help}</div>}
            {step.platform.connect.fields.map((f, i) => (
              <div className="field" key={f.key}>
                <label htmlFor={`f-${f.key}`}>
                  {f.label}
                  {!f.required && <span className="muted"> (optional)</span>}
                </label>
                <input
                  id={`f-${f.key}`}
                  className={"input" + (f.kind === "password" ? " mono" : "")}
                  type={f.kind === "password" ? "password" : "text"}
                  placeholder={f.placeholder}
                  value={fields[f.key] ?? ""}
                  autoFocus={i === 0}
                  autoComplete="off"
                  spellCheck={false}
                  onChange={(e) => setFields({ ...fields, [f.key]: e.target.value })}
                />
                {f.help && <span className="help">{f.help}</span>}
              </div>
            ))}
            {step.platform.connect.setupUrl && !step.platform.connect.needsApp && (
              <p>
                <button type="button" className="btn small" onClick={() => void api.openUrl(step.platform.connect.setupUrl!)}>
                  <ExternalLink size={14} /> Open {step.platform.id === "telegram" ? "@BotFather" : `${step.platform.name} settings`}
                </button>
              </p>
            )}
            {step.platform.notes.map((n, i) => (
              <p className="small muted" key={i}>
                {n}
              </p>
            ))}
            {step.platform.connect.needsApp && (
              <p className="small muted">
                Using your saved developer app.{" "}
                <a
                  href="#"
                  onClick={(e) => {
                    e.preventDefault();
                    setStep({ kind: "setup", platform: step.platform });
                  }}
                >
                  Change it
                </a>
              </p>
            )}
            {error && <div className="error-box">{error}</div>}
            <button type="submit" hidden />
          </form>
        )}

        {step.kind === "waiting" && (
          <div className="empty" style={{ padding: "24px 12px" }}>
            <Spinner big />
            <h2>Waiting for you in the browser</h2>
            <p>
              Approve access on the {step.platform.name} page that opened in your browser. This window updates by itself when you are done.
            </p>
            <div style={{ width: "100%", maxWidth: 560, textAlign: "left" }}>
              <span className="field-label">Browser didn't open? Copy this link into it:</span>
              <CopyField value={step.authUrl} label="Sign-in link" />
            </div>
          </div>
        )}

        {step.kind === "done" && (
          <div>
            <p style={{ marginTop: 0 }}>
              {step.accounts.length === 1 ? "This account is" : "These accounts are"} ready. Default streams were added to your Streams page.
            </p>
            {step.accounts.map((a) => (
              <div key={a.id} className="row" style={{ padding: "8px 0" }}>
                <AccountAvatar account={a} size={40} />
                <div>
                  <strong>{a.displayName}</strong>
                  <div className="small muted">{a.handle}</div>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
      <div className="modal-footer">
        {(step.kind === "setup" || step.kind === "form") && (
          <button className="btn ghost" onClick={() => setStep({ kind: "pick" })} disabled={busy}>
            <ArrowLeft size={15} /> All networks
          </button>
        )}
        <span className="spacer" />
        {step.kind === "waiting" && (
          <>
            <button className="btn" onClick={() => void api.openUrl(step.authUrl)}>
              <ExternalLink size={15} /> Open again
            </button>
            <button
              className="btn"
              onClick={() => {
                void api.cancelConnect();
              }}
            >
              Cancel
            </button>
          </>
        )}
        {step.kind === "form" && (
          <button className="btn primary" disabled={busy} onClick={() => void connect(step.platform)}>
            {busy ? <Spinner /> : step.platform.connect.method === "oauth" ? <ExternalLink size={15} /> : <Check size={15} />}
            {step.platform.connect.method === "oauth" ? "Continue in browser" : "Connect"}
          </button>
        )}
        {step.kind === "done" && (
          <>
            <button className="btn" onClick={() => setStep({ kind: "pick" })}>
              Connect another
            </button>
            <button className="btn primary" onClick={close}>
              Done
            </button>
          </>
        )}
        {step.kind === "pick" && (
          <button className="btn" onClick={close}>
            Close
          </button>
        )}
      </div>
    </Modal>
  );
}
