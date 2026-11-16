// Types mirroring the Rust core (connector-core). Keep in sync with
// src-tauri/core/src/{model,store,service}.rs and providers/mod.rs.

export type Platform =
  | "mastodon"
  | "bluesky"
  | "misskey"
  | "x"
  | "reddit"
  | "linkedin"
  | "facebook"
  | "tumblr"
  | "telegram"
  | "discord";

export type CountMode = "graphemes" | "mastodon" | "bluesky" | "twitter" | "utf16";

export interface FieldInfo {
  key: string;
  label: string;
  placeholder: string;
  kind: "text" | "password" | "url";
  required: boolean;
  help: string | null;
  defaultValue: string | null;
}

export interface ConnectInfo {
  method: "oauth" | "form";
  fields: FieldInfo[];
  needsApp: boolean;
  appFields: FieldInfo[];
  redirectHost: string;
  setupUrl: string | null;
  setupSteps: string[];
  help: string | null;
}

export interface Limits {
  maxChars: number;
  countMode: CountMode;
  maxCharsWithMedia: number | null;
  maxImages: number;
  imageMaxBytes: number;
  imageMaxDimension: number;
  imageTypes: string[];
  altTextMax: number;
  requiresMedia: boolean;
}

export interface Choice {
  value: string;
  label: string;
}

export interface Features {
  contentWarning: boolean;
  visibility: Choice[];
  sensitive: boolean;
  title: "none" | "optional" | "required";
  subreddit: boolean;
  link: boolean;
  tags: boolean;
  silent: boolean;
  spoiler: boolean;
  language: boolean;
  reply: boolean;
  like: boolean;
  repost: boolean;
  delete: boolean;
  inbox: boolean;
  stats: boolean;
  likeLabel: string;
  repostLabel: string;
}

export interface StreamKindInfo {
  id: string;
  label: string;
  param: { label: string; placeholder: string } | null;
  default: boolean;
  notifications: boolean;
  ownPosts: boolean;
}

export interface PlatformInfo {
  id: Platform;
  name: string;
  color: string;
  summary: string;
  connect: ConnectInfo;
  limits: Limits;
  features: Features;
  streams: StreamKindInfo[];
  notes: string[];
}

export interface AppCredentialInfo {
  platform: Platform;
  clientId: string;
  hasSecret: boolean;
  updatedAt: number;
}

export interface AppInfo {
  version: string;
  dataDir: string;
  secretBackend: "keyring" | "file";
  /** Name of the OS credential store ("Windows Credential Manager"...). */
  secretStoreName: string;
  platforms: PlatformInfo[];
  appCredentials: AppCredentialInfo[];
  redirectUris: Record<Platform, string>;
}

export interface StatsSnapshot {
  accountId: string;
  day: string;
  followers: number | null;
  following: number | null;
  posts: number | null;
}

export interface Account {
  id: string;
  platform: Platform;
  server: string;
  remoteId: string;
  handle: string;
  displayName: string;
  avatarUrl: string | null;
  profileUrl: string | null;
  meta: Record<string, unknown>;
  status: "ok" | "needsReconnect";
  statusMessage: string | null;
  position: number;
  createdAt: number;
  updatedAt: number;
  limits: Limits;
  stats: StatsSnapshot | null;
}

export interface Settings {
  theme: "system" | "light" | "dark";
  closeToTray: boolean;
  notifyPublished: boolean;
  notifyFailed: boolean;
  lateGraceMinutes: number;
  oauthPort: number;
  defaultRefreshMinutes: number;
  weekStartsMonday: boolean;
  lastAccounts: string[];
  trayHintShown: boolean;
  compactStreams: boolean;
}

export interface Stream {
  id: string;
  accountId: string;
  kind: string;
  param: string | null;
  title: string | null;
  position: number;
  refreshMinutes: number;
  createdAt: number;
}

export interface Author {
  name: string;
  handle: string;
  avatar: string | null;
  url: string | null;
}

export interface MediaView {
  kind: "image" | "video" | "gif" | "audio" | "other";
  url: string;
  previewUrl: string | null;
  alt: string | null;
  width: number | null;
  height: number | null;
}

export interface LinkCard {
  url: string;
  title: string | null;
  description: string | null;
  image: string | null;
}

export interface Counts {
  replies: number | null;
  reposts: number | null;
  likes: number | null;
  quotes: number | null;
  score: number | null;
}

export type NotificationKind =
  | "mention"
  | "reply"
  | "quote"
  | "like"
  | "repost"
  | "follow"
  | "followRequest"
  | "poll"
  | "update"
  | "message"
  | "other";

export interface FeedItem {
  id: string;
  accountId: string;
  url: string | null;
  author: Author;
  createdAt: string;
  text: string;
  html: string | null;
  title: string | null;
  contentWarning: string | null;
  sensitive: boolean;
  media: MediaView[];
  link: LinkCard | null;
  quote: FeedItem | null;
  repostedBy: Author | null;
  context: string | null;
  counts: Counts;
  viewer: { liked: boolean; reposted: boolean };
  actions: { like: boolean; repost: boolean; reply: boolean; delete: boolean };
  target: unknown;
  notification: { kind: NotificationKind; label: string; subject: string | null } | null;
}

export interface FeedPage {
  items: FeedItem[];
  cursor: string | null;
}

export type Interaction = "like" | "unlike" | "repost" | "unrepost";

export interface MediaAttachment {
  id: string;
  path: string;
  fileName: string;
  mime: string;
  size: number;
  width: number;
  height: number;
  alt: string;
}

export interface PostOptions {
  contentWarning?: string | null;
  visibility?: string | null;
  sensitive?: boolean;
  language?: string | null;
  title?: string | null;
  subreddit?: string | null;
  link?: string | null;
  tags?: string[];
  silent?: boolean;
  spoiler?: boolean;
}

export interface ReplyRef {
  accountId: string;
  target: unknown;
  authorHandle: string;
  textPreview: string;
  url: string | null;
}

export type PostStatus = "draft" | "scheduled" | "publishing" | "published" | "partial" | "failed" | "missed";
export type TargetStatus = "pending" | "publishing" | "published" | "failed";

export interface PostTarget {
  accountId: string;
  platform: Platform;
  accountLabel: string;
  bodyOverride: string | null;
  status: TargetStatus;
  remoteId: string | null;
  remoteUrl: string | null;
  error: string | null;
  publishedAt: number | null;
}

export interface Post {
  id: string;
  body: string;
  media: MediaAttachment[];
  options: PostOptions;
  replyTo: ReplyRef | null;
  status: PostStatus;
  scheduledAt: number | null;
  publishedAt: number | null;
  createdAt: number;
  updatedAt: number;
  lastError: string | null;
  targets: PostTarget[];
}

export interface TargetInput {
  accountId: string;
  bodyOverride: string | null;
}

export interface SavePostInput {
  id: string | null;
  body: string;
  media: MediaAttachment[];
  options: PostOptions;
  replyTo: ReplyRef | null;
  targets: TargetInput[];
  action: "draft" | "schedule" | "publish";
  scheduledAt: number | null;
}

export interface TargetProblem {
  accountId: string;
  problems: string[];
}

export interface InboxPage {
  items: FeedItem[];
  cursors: Record<string, string>;
  errors: { accountId: string; message: string }[];
  seen: Record<string, string>;
}

export interface AnalyticsOverview {
  accounts: { accountId: string; history: StatsSnapshot[]; published30d: number }[];
  daily: { day: string; count: number }[];
  scheduled: number;
  drafts: number;
  published30d: number;
  failed30d: number;
}

export interface ConnectStart {
  flowId: string;
  authUrl: string;
}

export interface AppErrorShape {
  kind: string;
  message: string;
}
