// Typed wrappers around the Tauri commands in src-tauri/src/commands.rs.

import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  AnalyticsOverview,
  AppErrorShape,
  AppInfo,
  ConnectStart,
  FeedPage,
  InboxPage,
  Interaction,
  MediaAttachment,
  Platform,
  Post,
  PostStatus,
  SavePostInput,
  Settings,
  Stream,
  TargetProblem,
} from "./types";

/** Turns whatever a command threw into a readable message. */
export function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) return String((e as AppErrorShape).message);
  return String(e);
}

export function errorKind(e: unknown): string | null {
  if (e && typeof e === "object" && "kind" in e) return String((e as AppErrorShape).kind);
  return null;
}

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  getSettings: () => invoke<Settings>("get_settings"),
  updateSettings: (patch: Partial<Settings>) => invoke<Settings>("update_settings", { patch }),
  setAppCredentials: (platform: Platform, clientId: string, clientSecret: string | null) =>
    invoke<void>("set_app_credentials", { platform, clientId, clientSecret }),
  clearAppCredentials: (platform: Platform) => invoke<void>("clear_app_credentials", { platform }),

  listAccounts: () => invoke<Account[]>("list_accounts"),
  beginConnect: (platform: Platform, fields: Record<string, string>) =>
    invoke<ConnectStart>("begin_connect", { platform, fields }),
  completeConnect: (flowId: string) => invoke<Account[]>("complete_connect", { flowId }),
  cancelConnect: () => invoke<void>("cancel_connect"),
  connectForm: (platform: Platform, fields: Record<string, string>) =>
    invoke<Account[]>("connect_form", { platform, fields }),
  removeAccount: (id: string) => invoke<void>("remove_account", { id }),
  refreshAccount: (id: string) => invoke<Account>("refresh_account", { id }),
  reorderAccounts: (ids: string[]) => invoke<void>("reorder_accounts", { ids }),

  listStreams: () => invoke<Stream[]>("list_streams"),
  addStream: (accountId: string, kind: string, param: string | null) =>
    invoke<Stream>("add_stream", { accountId, kind, param }),
  updateStream: (id: string, title: string | null, refreshMinutes: number) =>
    invoke<void>("update_stream", { id, title, refreshMinutes }),
  removeStream: (id: string) => invoke<void>("remove_stream", { id }),
  reorderStreams: (ids: string[]) => invoke<void>("reorder_streams", { ids }),
  loadStream: (streamId: string, cursor: string | null) => invoke<FeedPage>("load_stream", { streamId, cursor }),
  interact: (accountId: string, target: unknown, action: Interaction) =>
    invoke<unknown>("interact", { accountId, target, action }),
  deleteItem: (accountId: string, remoteId: string) => invoke<void>("delete_item", { accountId, remoteId }),
  ownPosts: (accountId: string) => invoke<FeedPage>("own_posts", { accountId }),

  loadInbox: (accountIds: string[] | null, cursors: Record<string, string> | null) =>
    invoke<InboxPage>("load_inbox", { accountIds, cursors }),
  markInboxSeen: (accountIds: string[] | null) => invoke<void>("mark_inbox_seen", { accountIds }),

  importMedia: (paths: string[]) => invoke<MediaAttachment[]>("import_media", { paths }),
  importMediaBytes: (name: string, bytes: Uint8Array) =>
    invoke<MediaAttachment>("import_media_bytes", bytes, { headers: { "x-file-name": name } }),

  listPosts: (statuses: PostStatus[], from: number | null = null, to: number | null = null) =>
    invoke<Post[]>("list_posts", { statuses, from, to }),
  getPost: (id: string) => invoke<Post>("get_post", { id }),
  validatePost: (input: SavePostInput) => invoke<TargetProblem[]>("validate_post", { input }),
  savePost: (input: SavePostInput) => invoke<Post>("save_post", { input }),
  publishPost: (id: string) => invoke<Post>("publish_post", { id }),
  deletePost: (id: string) => invoke<void>("delete_post", { id }),
  unschedulePost: (id: string) => invoke<Post>("unschedule_post", { id }),
  deleteRemote: (postId: string, accountId: string) => invoke<Post>("delete_remote", { postId, accountId }),

  analytics: () => invoke<AnalyticsOverview>("analytics"),
  openUrl: (url: string) => invoke<void>("open_url", { url }),
  getAutostart: () => invoke<boolean>("get_autostart"),
  setAutostart: (enabled: boolean) => invoke<boolean>("set_autostart", { enabled }),
  trayAvailable: () => invoke<boolean>("tray_available"),
  quit: () => invoke<void>("quit_app"),
};
