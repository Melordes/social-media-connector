// Analytics: headline numbers, posts published per day, follower trends
// per account and the best-performing recent posts.
//
// Chart rules follow the data-viz method: one series per chart (no legend
// needed, the title names it), one validated hue (#2a78d6 light / #3987e5
// dark), thin columns with 4px rounded tops on a single baseline, hairline
// solid gridlines, hover/focus tooltips, and a table view for every chart.

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ArrowDownRight, ArrowUpRight, BarChart3, ExternalLink, Minus } from "lucide-react";
import type { Account, AnalyticsOverview, FeedItem, StatsSnapshot } from "../lib/types";
import { api, errorMessage } from "../lib/api";
import { useApp } from "../lib/store";
import { formatCount, preview } from "../lib/text";
import { fmt } from "../lib/format";
import { AccountAvatar, EmptyState, Spinner } from "../components/common";

function niceMax(v: number): number {
  if (v <= 4) return 4;
  const pow = Math.pow(10, Math.floor(Math.log10(v)));
  for (const m of [1, 2, 2.5, 5, 10]) {
    if (m * pow >= v) return m * pow;
  }
  return 10 * pow;
}

function useWidth<T extends HTMLElement>(): [React.RefObject<T | null>, number] {
  const ref = useRef<T>(null);
  const [w, setW] = useState(600);
  useLayoutEffect(() => {
    if (!ref.current) return;
    const ro = new ResizeObserver((entries) => setW(entries[0].contentRect.width));
    ro.observe(ref.current);
    return () => ro.disconnect();
  }, []);
  return [ref, w];
}

function ColumnChart({ data, label }: { data: { key: string; label: string; value: number }[]; label: string }) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const [tip, setTip] = useState<{ x: number; y: number; text: string; value: string } | null>(null);
  const height = 180;
  const axisH = 22;
  const left = 34;
  const max = niceMax(Math.max(...data.map((d) => d.value), 0));
  const plotW = Math.max(100, width - left);
  const band = plotW / Math.max(1, data.length);
  const barW = Math.min(24, Math.max(4, band - 2));
  const ticks = [0, max / 4, max / 2, (3 * max) / 4, max].filter((t) => Number.isInteger(t));
  const y = (v: number) => height - (v / max) * height;
  const labelEvery = Math.ceil(data.length / Math.max(2, Math.floor(plotW / 64)));

  const show = (e: React.MouseEvent | React.FocusEvent, d: (typeof data)[number]) => {
    const r = (e.currentTarget as SVGElement).getBoundingClientRect();
    setTip({ x: r.left + r.width / 2, y: r.top, text: d.label, value: String(d.value) });
  };

  return (
    <div className="chart" ref={ref} style={{ position: "relative" }}>
      <svg height={height + axisH} role="img" aria-label={label}>
        <g className="grid">
          {ticks.map((t) => (
            <line key={t} x1={left} x2={left + plotW} y1={y(t)} y2={y(t)} strokeWidth={1} />
          ))}
        </g>
        <g className="axis">
          {ticks.map((t) => (
            <text key={t} x={left - 8} y={y(t) + 4} textAnchor="end" style={{ fontVariantNumeric: "tabular-nums" }}>
              {formatCount(t)}
            </text>
          ))}
          {data.map((d, i) =>
            (data.length - 1 - i) % labelEvery === 0 ? (
              <text key={d.key} x={left + band * i + band / 2} y={height + 16} textAnchor="middle">
                {d.label.split(",")[0]}
              </text>
            ) : null,
          )}
        </g>
        {data.map((d, i) => {
          const x = left + band * i + (band - barW) / 2;
          const h = height - y(d.value);
          const r = Math.min(4, h);
          return (
            <g key={d.key}>
              {d.value > 0 && (
                <path
                  className="bar"
                  d={`M${x},${height} V${height - h + r} Q${x},${height - h} ${x + r},${height - h} H${x + barW - r} Q${x + barW},${height - h} ${x + barW},${height - h + r} V${height} Z`}
                  style={{ fill: "var(--series-1)" }}
                />
              )}
              <rect
                x={left + band * i}
                y={0}
                width={band}
                height={height}
                fill="transparent"
                tabIndex={0}
                aria-label={`${d.label}: ${d.value}`}
                onMouseEnter={(e) => show(e, d)}
                onFocus={(e) => show(e, d)}
                onMouseLeave={() => setTip(null)}
                onBlur={() => setTip(null)}
              />
            </g>
          );
        })}
        <line x1={left} x2={left + plotW} y1={height} y2={height} style={{ stroke: "var(--border-strong)" }} strokeWidth={1} />
      </svg>
      {tip && (
        <div className="chart-tip" style={{ left: tip.x, top: tip.y }}>
          <strong>{tip.value}</strong> <span style={{ opacity: 0.75 }}>{tip.text}</span>
        </div>
      )}
    </div>
  );
}

function Sparkline({ points }: { points: number[] }) {
  if (points.length < 2) return null;
  const w = 120;
  const h = 34;
  const min = Math.min(...points);
  const max = Math.max(...points);
  const span = max - min || 1;
  const xy = points.map((p, i) => [2 + (i / (points.length - 1)) * (w - 8), 4 + (1 - (p - min) / span) * (h - 8)]);
  const last = xy[xy.length - 1];
  return (
    <svg width={w} height={h} aria-hidden="true" style={{ overflow: "visible" }}>
      <polyline
        points={xy.map((p) => p.join(",")).join(" ")}
        fill="none"
        stroke="var(--chart-muted)"
        strokeWidth={2}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
      <circle cx={last[0]} cy={last[1]} r={4} fill="var(--series-1)" stroke="var(--surface)" strokeWidth={2} />
    </svg>
  );
}

function Delta({ history, days }: { history: StatsSnapshot[]; days: number }) {
  const withFollowers = history.filter((h) => h.followers != null);
  if (withFollowers.length < 2) return null;
  const latest = withFollowers[withFollowers.length - 1];
  const cutoff = new Date(Date.now() - days * 86400_000).toISOString().slice(0, 10);
  const base = withFollowers.find((h) => h.day >= cutoff) ?? withFollowers[0];
  if (base === latest) return null;
  const diff = (latest.followers ?? 0) - (base.followers ?? 0);
  const Icon = diff > 0 ? ArrowUpRight : diff < 0 ? ArrowDownRight : Minus;
  return (
    <span className={"small delta " + (diff > 0 ? "up" : diff < 0 ? "down" : "")} style={{ display: "inline-flex", alignItems: "center", gap: 2 }}>
      <Icon size={14} aria-hidden="true" />
      {diff > 0 ? "+" : ""}
      {diff.toLocaleString()} <span className="muted">since {fmt.shortDate(Date.parse(base.day))}</span>
    </span>
  );
}

function AccountTile({ account, history, published }: { account: Account; history: StatsSnapshot[]; published: number }) {
  const latest = [...history].reverse().find((h) => h.followers != null);
  const points = history.filter((h) => h.followers != null).slice(-30).map((h) => h.followers!);
  return (
    <div className="card tile">
      <div className="row">
        <AccountAvatar account={account} size={34} />
        <div className="grow" style={{ minWidth: 0 }}>
          <strong style={{ display: "block", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{account.displayName}</strong>
          <span className="small muted">{account.handle}</span>
        </div>
      </div>
      <div className="row" style={{ marginTop: 10, alignItems: "flex-end" }}>
        <div className="grow">
          <div className="label">{account.platform === "telegram" ? "Subscribers" : "Followers"}</div>
          <div className="value">{latest?.followers != null ? latest.followers.toLocaleString() : "–"}</div>
          <Delta history={history} days={30} />
        </div>
        <Sparkline points={points} />
      </div>
      <div className="small muted" style={{ marginTop: 8 }}>
        {published} post{published === 1 ? "" : "s"} sent from this app in the last 30 days
        {latest?.posts != null ? ` · ${formatCount(latest.posts)} posts in total` : ""}
      </div>
      {points.length < 2 && <div className="small faint" style={{ marginTop: 4 }}>The trend fills in as the app records counts once a day.</div>}
    </div>
  );
}

function engagement(i: FeedItem): number {
  return (i.counts.likes ?? 0) + (i.counts.reposts ?? 0) + (i.counts.replies ?? 0) + (i.counts.quotes ?? 0) + Math.max(0, i.counts.score ?? 0);
}

function TopPosts({ accounts }: { accounts: Account[] }) {
  const info = useApp((s) => s.info);
  const eligible = accounts.filter((a) => info?.platforms.find((p) => p.id === a.platform)?.streams.some((s) => s.ownPosts));
  const [accountId, setAccountId] = useState(eligible[0]?.id ?? "");
  const [items, setItems] = useState<FeedItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!accountId) return;
    setItems(null);
    setError(null);
    api
      .ownPosts(accountId)
      .then((p) => setItems(p.items.filter((i) => !i.repostedBy).sort((a, b) => engagement(b) - engagement(a)).slice(0, 10)))
      .catch((e) => setError(errorMessage(e)));
  }, [accountId]);
  if (!eligible.length) return null;
  return (
    <div className="card chart-card">
      <div className="row" style={{ marginBottom: 10 }}>
        <div className="grow">
          <h3>Top recent posts</h3>
          <p className="sub" style={{ margin: 0 }}>
            The account's latest posts, ranked by replies, reposts and likes.
          </p>
        </div>
        <select className="select" style={{ width: "auto" }} value={accountId} onChange={(e) => setAccountId(e.target.value)} aria-label="Account">
          {eligible.map((a) => (
            <option key={a.id} value={a.id}>
              {a.displayName} ({a.handle})
            </option>
          ))}
        </select>
      </div>
      {error ? (
        <div className="error-box">{error}</div>
      ) : items === null ? (
        <Spinner />
      ) : items.length === 0 ? (
        <p className="muted">No posts yet.</p>
      ) : (
        <table className="data">
          <thead>
            <tr>
              <th>Post</th>
              <th className="num">Replies</th>
              <th className="num">Reposts</th>
              <th className="num">Likes</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {items.map((i) => (
              <tr key={i.id}>
                <td>
                  <div>{preview(i.title || i.text, 110) || <em className="muted">(media only)</em>}</div>
                  <div className="small faint">{fmt.dateTime(Date.parse(i.createdAt))}</div>
                </td>
                <td className="num">{i.counts.replies ?? "–"}</td>
                <td className="num">{i.counts.reposts ?? "–"}</td>
                <td className="num">{i.counts.score ?? i.counts.likes ?? "–"}</td>
                <td>
                  {i.url && (
                    <button className="icon-btn" aria-label="Open post" onClick={() => void api.openUrl(i.url!)}>
                      <ExternalLink size={15} />
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

export function AnalyticsPage() {
  const accounts = useApp((s) => s.accounts);
  const postsVersion = useApp((s) => s.postsVersion);
  const toast = useApp((s) => s.toast);
  const [data, setData] = useState<AnalyticsOverview | null>(null);
  const [asTable, setAsTable] = useState(false);

  useEffect(() => {
    api
      .analytics()
      .then(setData)
      .catch((e) => toast(errorMessage(e), "error"));
  }, [postsVersion, accounts.length, toast]);

  const daily = useMemo(
    () => (data?.daily ?? []).map((d) => ({ key: d.day, label: fmt.shortDate(Date.parse(d.day + "T12:00:00")), value: d.count })),
    [data],
  );

  return (
    <>
      <header className="page-header">
        <h1>Analytics</h1>
      </header>
      <div className="page-body viz-root">
        {!data ? (
          <div className="empty">
            <Spinner big />
          </div>
        ) : accounts.length === 0 ? (
          <EmptyState icon={<BarChart3 size={28} />} title="Nothing to measure yet">
            <p>Connect accounts and publish a few posts; follower counts and posting activity appear here.</p>
          </EmptyState>
        ) : (
          <>
            <div className="tiles">
              {[
                ["Scheduled", data.scheduled],
                ["Drafts", data.drafts],
                ["Sent in the last 30 days", data.published30d],
                ["Failed or missed, last 30 days", data.failed30d],
              ].map(([label, value]) => (
                <div className="card tile" key={label as string}>
                  <div className="label">{label}</div>
                  <div className="value">{(value as number).toLocaleString()}</div>
                </div>
              ))}
            </div>
            <div className="card chart-card">
              <div className="row">
                <div className="grow">
                  <h3>Posts sent per day</h3>
                  <p className="sub">Each account a post reached counts once, over the last 30 days.</p>
                </div>
                <div className="segmented" role="radiogroup" aria-label="Display">
                  <button className={!asTable ? "active" : ""} onClick={() => setAsTable(false)} role="radio" aria-checked={!asTable}>
                    Chart
                  </button>
                  <button className={asTable ? "active" : ""} onClick={() => setAsTable(true)} role="radio" aria-checked={asTable}>
                    Table
                  </button>
                </div>
              </div>
              {asTable ? (
                <div style={{ maxHeight: 260, overflow: "auto" }}>
                  <table className="data">
                    <thead>
                      <tr>
                        <th>Day</th>
                        <th className="num">Posts sent</th>
                      </tr>
                    </thead>
                    <tbody>
                      {[...daily].reverse().map((d) => (
                        <tr key={d.key}>
                          <td>{fmt.day(Date.parse(d.key + "T12:00:00"))}</td>
                          <td className="num">{d.value}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              ) : (
                <ColumnChart data={daily} label="Posts sent per day over the last 30 days" />
              )}
            </div>
            <h2 className="section-title" style={{ margin: "6px 0 10px" }}>
              Audience
            </h2>
            <div className="account-stats">
              {accounts.map((a) => {
                const entry = data.accounts.find((x) => x.accountId === a.id);
                return <AccountTile key={a.id} account={a} history={entry?.history ?? []} published={entry?.published30d ?? 0} />;
              })}
            </div>
            <TopPosts accounts={accounts} />
          </>
        )}
      </div>
    </>
  );
}
