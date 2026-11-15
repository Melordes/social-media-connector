// Small shared building blocks: platform icons, avatars, modal, menu,
// toasts, lightbox, switch.

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { siBluesky, siDiscord, siFacebook, siMastodon, siMisskey, siReddit, siTelegram, siTumblr, siX } from "simple-icons";
import { X as XIcon } from "lucide-react";
import type { Account, Author, Platform } from "../lib/types";
import { useApp } from "../lib/store";

const ICONS: Partial<Record<Platform, { path: string }>> = {
  mastodon: siMastodon,
  bluesky: siBluesky,
  misskey: siMisskey,
  x: siX,
  reddit: siReddit,
  tumblr: siTumblr,
  telegram: siTelegram,
  discord: siDiscord,
  facebook: siFacebook,
};

export const PLATFORM_COLORS: Record<Platform, string> = {
  mastodon: "#6364FF",
  bluesky: "#1185FE",
  misskey: "#86B300",
  x: "#0F1419",
  reddit: "#FF4500",
  linkedin: "#0A66C2",
  facebook: "#0866FF",
  tumblr: "#36465D",
  telegram: "#26A5E4",
  discord: "#5865F2",
};

export function PlatformIcon({ platform, size = 18, title }: { platform: Platform; size?: number; title?: string }) {
  const icon = ICONS[platform];
  const inner = Math.round(size * 0.58);
  return (
    <span
      className="platform-icon"
      style={{ width: size, height: size, background: PLATFORM_COLORS[platform] }}
      title={title}
      aria-label={title}
      role={title ? "img" : undefined}
    >
      {icon ? (
        <svg width={inner} height={inner} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
          <path d={icon.path} />
        </svg>
      ) : (
        <span className="li" style={{ fontSize: Math.round(size * 0.5) }}>
          in
        </span>
      )}
    </span>
  );
}

function initials(name: string): string {
  const parts = name.replace(/[@_.-]/g, " ").trim().split(/\s+/).filter(Boolean);
  return ((parts[0]?.[0] ?? "?") + (parts[1]?.[0] ?? "")).toUpperCase();
}

export function Avatar({
  src,
  name,
  size = 40,
  platform,
  warn,
  title,
}: {
  src: string | null | undefined;
  name: string;
  size?: number;
  platform?: Platform;
  warn?: boolean;
  title?: string;
}) {
  const [failed, setFailed] = useState(false);
  useEffect(() => setFailed(false), [src]);
  const badge = Math.max(14, Math.round(size * 0.42));
  return (
    <span className={"avatar" + (warn ? " warn" : "")} style={{ width: size, height: size, fontSize: size * 0.36 }} title={title}>
      {src && !failed ? (
        <img className="avatar-img" src={src} alt="" loading="lazy" referrerPolicy="no-referrer" onError={() => setFailed(true)} />
      ) : (
        <span aria-hidden="true">{initials(name)}</span>
      )}
      {platform && (
        <span className="avatar-badge">
          <PlatformIcon platform={platform} size={badge} />
        </span>
      )}
    </span>
  );
}

export function AccountAvatar({ account, size = 40 }: { account: Account; size?: number }) {
  return (
    <Avatar
      src={account.avatarUrl}
      name={account.displayName}
      size={size}
      platform={account.platform}
      warn={account.status !== "ok"}
      title={`${account.displayName} (${account.handle})`}
    />
  );
}

export function AuthorAvatar({ author, size = 40 }: { author: Author; size?: number }) {
  return <Avatar src={author.avatar} name={author.name || author.handle} size={size} />;
}

export function Modal({
  title,
  onClose,
  children,
  footer,
  size,
  closeOnOverlay = true,
  icon,
}: {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  size?: "wide" | "medium";
  closeOnOverlay?: boolean;
  icon?: ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return createPortal(
    <div
      className="overlay"
      onMouseDown={(e) => {
        if (closeOnOverlay && e.target === e.currentTarget) onClose();
      }}
    >
      <div className={"modal" + (size ? " " + size : "")} role="dialog" aria-modal="true" aria-label={typeof title === "string" ? title : undefined}>
        <div className="modal-header">
          {icon}
          <h2>{title}</h2>
          <button className="icon-btn" onClick={onClose} aria-label="Close">
            <XIcon size={18} />
          </button>
        </div>
        {children}
        {footer && <div className="modal-footer">{footer}</div>}
      </div>
    </div>,
    document.body,
  );
}

export function Confirm({
  title,
  message,
  confirmLabel,
  danger,
  onConfirm,
  onCancel,
}: {
  title: string;
  message: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <Modal
      title={title}
      onClose={onCancel}
      footer={
        <>
          <span className="spacer" />
          <button className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button className={"btn " + (danger ? "danger" : "primary")} onClick={onConfirm} autoFocus>
            {confirmLabel}
          </button>
        </>
      }
    >
      <div className="modal-body">{message}</div>
    </Modal>
  );
}

export interface MenuItem {
  label: string;
  icon?: ReactNode;
  onClick: () => void;
  danger?: boolean;
  disabled?: boolean;
  separatorBefore?: boolean;
}

/** A button that opens a small menu below it. */
export function MenuButton({ items, label, children }: { items: MenuItem[]; label: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const btn = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ top: 0, left: 0 });
  useLayoutEffect(() => {
    if (!open || !btn.current) return;
    const r = btn.current.getBoundingClientRect();
    const width = menu.current?.offsetWidth ?? 200;
    const height = menu.current?.offsetHeight ?? 200;
    const left = Math.min(Math.max(8, r.right - width), window.innerWidth - width - 8);
    const top = r.bottom + 4 + height > window.innerHeight ? Math.max(8, r.top - height - 4) : r.bottom + 4;
    setPos({ top, left });
  }, [open]);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (!menu.current?.contains(e.target as Node) && !btn.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);
  return (
    <>
      <button ref={btn} className="icon-btn" aria-label={label} title={label} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        {children}
      </button>
      {open &&
        createPortal(
          <div ref={menu} className="menu" role="menu" style={{ position: "fixed", top: pos.top, left: pos.left }}>
            {items.map((it, i) => (
              <div key={i}>
                {it.separatorBefore && <hr />}
                <button
                  role="menuitem"
                  className={it.danger ? "danger" : ""}
                  disabled={it.disabled}
                  onClick={() => {
                    setOpen(false);
                    it.onClick();
                  }}
                >
                  {it.icon}
                  {it.label}
                </button>
              </div>
            ))}
          </div>,
          document.body,
        )}
    </>
  );
}

export function Toasts() {
  const toasts = useApp((s) => s.toasts);
  const dismiss = useApp((s) => s.dismissToast);
  return (
    <div className="toasts" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={"toast " + t.kind} role={t.kind === "error" ? "alert" : "status"}>
          <p>{t.text}</p>
          <button className="icon-btn" style={{ width: 22, height: 22 }} onClick={() => dismiss(t.id)} aria-label="Dismiss">
            <XIcon size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}

export function Switch({ checked, onChange, label }: { checked: boolean; onChange: (v: boolean) => void; label: string }) {
  return (
    <label className="switch" title={label}>
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} aria-label={label} />
      <span />
    </label>
  );
}

export function Lightbox({ src, alt, onClose }: { src: string; alt: string | null; onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return createPortal(
    <div className="lightbox" onClick={onClose} role="dialog" aria-label="Image">
      <button className="icon-btn close" aria-label="Close" onClick={onClose}>
        <XIcon size={22} />
      </button>
      <div style={{ display: "flex", flexDirection: "column", alignItems: "center" }}>
        <img src={src} alt={alt ?? ""} referrerPolicy="no-referrer" />
        {alt && <div className="caption">{alt}</div>}
      </div>
    </div>,
    document.body,
  );
}

export function Spinner({ big }: { big?: boolean }) {
  return <span className={"spinner" + (big ? " big" : "")} role="status" aria-label="Loading" />;
}

export function EmptyState({ icon, title, children }: { icon: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <div className="empty-icon">{icon}</div>
      <h2>{title}</h2>
      {children}
    </div>
  );
}
