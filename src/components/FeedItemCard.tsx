// One post or notification in a stream or the inbox.

import { memo, useMemo, useState } from "react";
import {
  AtSign,
  BarChart3,
  Bell,
  ExternalLink,
  Heart,
  MessageCircle,
  MoreHorizontal,
  Pencil,
  Play,
  Quote,
  Repeat2,
  Reply,
  Trash2,
  UserPlus,
  ArrowBigUp,
} from "lucide-react";
import type { Account, FeedItem, NotificationKind } from "../lib/types";
import { api, errorMessage } from "../lib/api";
import { useApp, usePlatform } from "../lib/store";
import { linkify, sanitizePostHtml } from "../lib/sanitize";
import { formatCount } from "../lib/text";
import { relativeTime, fmt } from "../lib/format";
import { AuthorAvatar, Confirm, Lightbox, MenuButton, PlatformIcon } from "./common";

const NOTIF_ICON: Record<NotificationKind, typeof Heart> = {
  mention: AtSign,
  reply: Reply,
  quote: Quote,
  like: Heart,
  repost: Repeat2,
  follow: UserPlus,
  followRequest: UserPlus,
  poll: BarChart3,
  update: Pencil,
  message: MessageCircle,
  other: Bell,
};

function hostOf(url: string): string {
  try {
    return new URL(url).host.replace(/^www\./, "");
  } catch {
    return url;
  }
}

/** The id a network needs to delete one of our own items. */
export function remoteIdOf(item: FeedItem): string | null {
  const t = item.target as Record<string, unknown> | null;
  if (!t || typeof t !== "object") return null;
  for (const k of ["remoteId", "uri", "id", "fullname", "name"]) {
    const v = t[k];
    if (typeof v === "string" && v) return v;
  }
  return null;
}

function Content({ item, clamp }: { item: FeedItem; clamp?: boolean }) {
  const html = useMemo(() => (item.html ? sanitizePostHtml(item.html) : item.text ? linkify(item.text) : ""), [item.html, item.text]);
  if (!html) return null;
  return <div className={"item-text" + (clamp ? " clamped" : "")} dangerouslySetInnerHTML={{ __html: html }} />;
}

function Media({ item, reveal }: { item: FeedItem; reveal: boolean }) {
  const [shown, setShown] = useState<{ src: string; alt: string | null } | null>(null);
  const [unblurred, setUnblurred] = useState(false);
  if (!item.media.length) return null;
  const media = item.media.slice(0, 4);
  const blur = item.sensitive && !unblurred && !reveal;
  return (
    <>
      <div className={"media-grid n" + media.length}>
        {media.map((m, i) => {
          const preview = m.previewUrl || m.url;
          const isVideo = m.kind === "video" || m.kind === "gif" || m.kind === "audio";
          return (
            <button
              key={i}
              className={"media-tile" + (blur ? " blurred" : "")}
              title={m.alt ?? undefined}
              onClick={() => {
                if (blur) return setUnblurred(true);
                if (isVideo || m.kind === "other") {
                  if (item.url) void api.openUrl(item.url);
                } else setShown({ src: m.url, alt: m.alt });
              }}
            >
              {preview ? <img src={preview} alt={m.alt ?? ""} loading="lazy" referrerPolicy="no-referrer" /> : null}
              {isVideo && !blur && (
                <span className="play">
                  <Play size={30} fill="currentColor" />
                </span>
              )}
              {m.kind === "gif" && <span className="media-label">GIF</span>}
              {m.alt && m.kind === "image" && !blur && <span className="media-label">ALT</span>}
              {blur && <span className="play">Sensitive — click to show</span>}
            </button>
          );
        })}
      </div>
      {item.media.length > 4 && <div className="small muted" style={{ marginTop: 4 }}>+{item.media.length - 4} more</div>}
      {shown && <Lightbox src={shown.src} alt={shown.alt} onClose={() => setShown(null)} />}
    </>
  );
}

function LinkCardView({ card }: { card: NonNullable<FeedItem["link"]> }) {
  return (
    <button className="link-card" onClick={() => void api.openUrl(card.url)} title={card.url}>
      {card.image && <img src={card.image} alt="" loading="lazy" referrerPolicy="no-referrer" />}
      <div>
        <div className="host">{hostOf(card.url)}</div>
        {card.title && <div className="title">{card.title}</div>}
        {card.description && <div className="desc">{card.description}</div>}
      </div>
    </button>
  );
}

interface Props {
  item: FeedItem;
  account: Account;
  unread?: boolean;
  showAccount?: boolean;
  onChange?: (item: FeedItem) => void;
  onRemove?: (item: FeedItem) => void;
}

function FeedItemCardInner({ item, account, unread, showAccount, onChange, onRemove }: Props) {
  const platform = usePlatform(account.platform);
  const openComposer = useApp((s) => s.openComposer);
  const toast = useApp((s) => s.toast);
  const [showCw, setShowCw] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const notif = item.notification;
  const isPostLike = !notif || ["mention", "reply", "quote", "other", "update", "message"].includes(notif.kind);
  const NotifIcon = notif ? NOTIF_ICON[notif.kind] : null;
  const likeLabel = platform?.features.likeLabel ?? "Like";
  const repostLabel = platform?.features.repostLabel ?? "Repost";
  const LikeIcon = account.platform === "reddit" ? ArrowBigUp : Heart;

  const toggle = async (kind: "like" | "repost") => {
    const on = kind === "like" ? item.viewer.liked : item.viewer.reposted;
    const action = kind === "like" ? (on ? "unlike" : "like") : on ? "unrepost" : "repost";
    setBusy(kind);
    try {
      const target = await api.interact(account.id, item.target, action);
      const counts = { ...item.counts };
      if (kind === "like") {
        if (counts.likes != null) counts.likes = Math.max(0, counts.likes + (on ? -1 : 1));
        if (counts.score != null) counts.score = counts.score + (on ? -1 : 1);
      } else if (counts.reposts != null) counts.reposts = Math.max(0, counts.reposts + (on ? -1 : 1));
      onChange?.({
        ...item,
        target,
        counts,
        viewer: { ...item.viewer, [kind === "like" ? "liked" : "reposted"]: !on },
      });
    } catch (e) {
      toast(errorMessage(e), "error");
    } finally {
      setBusy(null);
    }
  };

  const doDelete = async () => {
    setConfirmDelete(false);
    const id = remoteIdOf(item);
    if (!id) return;
    try {
      await api.deleteItem(account.id, id);
      toast("Post deleted.", "success");
      onRemove?.(item);
    } catch (e) {
      toast(errorMessage(e), "error");
    }
  };

  const created = Date.parse(item.createdAt);
  const timeTitle = Number.isNaN(created) ? undefined : fmt.dateTime(created);

  return (
    <article className={"item" + (unread ? " unread" : "")}>
      <button
        className="icon-btn"
        style={{ width: 40, height: 40, padding: 0, borderRadius: "50%" }}
        onClick={() => item.author.url && void api.openUrl(item.author.url)}
        aria-label={`Open ${item.author.name}'s profile`}
        title={item.author.handle}
      >
        <AuthorAvatar author={item.author} size={40} />
      </button>
      <div className="item-main">
        {item.repostedBy && (
          <div className="item-repost">
            <Repeat2 size={13} /> {item.repostedBy.name} {repostLabel === "Boost" ? "boosted" : "reposted"}
          </div>
        )}
        {notif && NotifIcon && (
          <div className="notif-line">
            <NotifIcon size={14} />
            <span>
              <strong>{item.author.name}</strong> {notif.label}
            </span>
            <span className="grow" />
            {showAccount && <PlatformIcon platform={account.platform} size={14} title={`${account.displayName} (${account.handle})`} />}
            <button className="item-time" title={timeTitle} onClick={() => item.url && void api.openUrl(item.url)}>
              {relativeTime(item.createdAt)}
            </button>
          </div>
        )}
        {(!notif || isPostLike) && (
          <div className="item-head">
            <button className="item-name" onClick={() => item.author.url && void api.openUrl(item.author.url)}>
              {item.author.name}
            </button>
            <span className="item-handle">{item.author.handle}</span>
            {!notif && (
              <button className="item-time" title={timeTitle} onClick={() => item.url && void api.openUrl(item.url)}>
                {relativeTime(item.createdAt)}
              </button>
            )}
          </div>
        )}
        {item.context && <div className="item-context">{item.context}</div>}
        {item.title && <div className="item-title">{item.title}</div>}
        {item.contentWarning && !showCw ? (
          <div className="cw">
            <span>{item.contentWarning}</span>
            <button className="btn small" onClick={() => setShowCw(true)}>
              Show
            </button>
          </div>
        ) : (
          <>
            {isPostLike && <Content item={item} clamp />}
            {isPostLike && <Media item={item} reveal={showCw} />}
            {isPostLike && item.link && <LinkCardView card={item.link} />}
            {isPostLike && item.quote && (
              <div className="quote">
                <div className="item-head">
                  <strong className="item-name" style={{ cursor: "default" }}>
                    {item.quote.author.name}
                  </strong>
                  <span className="item-handle">{item.quote.author.handle}</span>
                </div>
                <Content item={item.quote} clamp />
                <Media item={item.quote} reveal={false} />
              </div>
            )}
          </>
        )}
        {notif?.subject && <div className="notif-subject">{notif.subject}</div>}
        {isPostLike && (item.actions.reply || item.actions.like || item.actions.repost || item.url) && (
          <div className="actions">
            {item.actions.reply && (
              <button className="action" onClick={() => openComposer({ replyTo: { item, account } })} title="Reply">
                <MessageCircle size={15} />
                {formatCount(item.counts.replies)}
              </button>
            )}
            {item.actions.repost && (
              <button
                className={"action repost" + (item.viewer.reposted ? " on" : "")}
                disabled={busy === "repost"}
                onClick={() => void toggle("repost")}
                title={item.viewer.reposted ? `Undo ${repostLabel.toLowerCase()}` : repostLabel}
                aria-pressed={item.viewer.reposted}
              >
                <Repeat2 size={16} />
                {formatCount(item.counts.reposts)}
              </button>
            )}
            {item.actions.like && (
              <button
                className={"action like" + (item.viewer.liked ? " on" : "")}
                disabled={busy === "like"}
                onClick={() => void toggle("like")}
                title={item.viewer.liked ? `Undo ${likeLabel.toLowerCase()}` : likeLabel}
                aria-pressed={item.viewer.liked}
              >
                <LikeIcon size={15} fill={item.viewer.liked ? "currentColor" : "none"} />
                {item.counts.score != null ? formatCount(item.counts.score) : formatCount(item.counts.likes)}
              </button>
            )}
            {!item.actions.like && item.counts.likes != null && item.counts.likes > 0 && (
              <span className="action" style={{ cursor: "default" }} title="Reactions">
                <Heart size={15} /> {formatCount(item.counts.likes)}
              </span>
            )}
            <span style={{ flex: 1 }} />
            {(item.url || item.actions.delete) && (
              <MenuButton
                label="More"
                items={[
                  ...(item.url
                    ? [{ label: "Open in browser", icon: <ExternalLink size={15} />, onClick: () => void api.openUrl(item.url!) }]
                    : []),
                  ...(item.url
                    ? [
                        {
                          label: "Copy link",
                          icon: <ExternalLink size={15} />,
                          onClick: () => {
                            void navigator.clipboard.writeText(item.url!).then(() => toast("Link copied."));
                          },
                        },
                      ]
                    : []),
                  ...(item.actions.delete && remoteIdOf(item)
                    ? [{ label: "Delete post", icon: <Trash2 size={15} />, danger: true, separatorBefore: true, onClick: () => setConfirmDelete(true) }]
                    : []),
                ]}
              >
                <MoreHorizontal size={16} />
              </MenuButton>
            )}
          </div>
        )}
      </div>
      {confirmDelete && (
        <Confirm
          title="Delete this post?"
          message={`It will be removed from ${platform?.name ?? "the network"} for everyone. This can't be undone.`}
          confirmLabel="Delete"
          danger
          onConfirm={() => void doDelete()}
          onCancel={() => setConfirmDelete(false)}
        />
      )}
    </article>
  );
}

export const FeedItemCard = memo(FeedItemCardInner);
