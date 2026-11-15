// Global app state: platform info, accounts, settings, streams, toasts and
// the composer. Pages fetch their own data and refresh on `postsVersion`.

import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import { api, errorMessage } from "./api";
import type { Account, AppInfo, FeedItem, Platform, PlatformInfo, Post, ReplyRef, Settings, Stream } from "./types";

export type Page = "streams" | "planner" | "inbox" | "analytics" | "accounts" | "settings";

export interface Toast {
  id: number;
  kind: "info" | "success" | "error";
  text: string;
}

export interface ComposerSeed {
  post?: Post;
  replyTo?: { item: FeedItem; account: Account };
  scheduledAt?: number;
  accountIds?: string[];
}

interface State {
  ready: boolean;
  loadError: string | null;
  info: AppInfo | null;
  accounts: Account[];
  settings: Settings | null;
  streams: Stream[];
  page: Page;
  postsVersion: number;
  toasts: Toast[];
  composer: ComposerSeed | null;
  connectOpen: Platform | true | null;
  unread: number;

  init: () => Promise<void>;
  setPage: (p: Page) => void;
  reloadInfo: () => Promise<void>;
  reloadAccounts: () => Promise<void>;
  reloadStreams: () => Promise<void>;
  updateSettings: (patch: Partial<Settings>) => Promise<void>;
  toast: (text: string, kind?: Toast["kind"]) => void;
  dismissToast: (id: number) => void;
  openComposer: (seed?: ComposerSeed) => void;
  closeComposer: () => void;
  openConnect: (platform?: Platform) => void;
  closeConnect: () => void;
  setUnread: (n: number) => void;
}

let toastId = 1;

export const useApp = create<State>((set, get) => ({
  ready: false,
  loadError: null,
  info: null,
  accounts: [],
  settings: null,
  streams: [],
  page: "streams",
  postsVersion: 0,
  toasts: [],
  composer: null,
  connectOpen: null,
  unread: 0,

  init: async () => {
    try {
      const [info, accounts, settings, streams] = await Promise.all([
        api.appInfo(),
        api.listAccounts(),
        api.getSettings(),
        api.listStreams(),
      ]);
      set({ info, accounts, settings, streams, ready: true });
      applyTheme(settings.theme);
      await listen("accounts-changed", () => {
        void get().reloadAccounts();
        void get().reloadStreams();
      });
      await listen("posts-changed", () => set((s) => ({ postsVersion: s.postsVersion + 1 })));
      await listen("open-composer", () => get().openComposer());
    } catch (e) {
      set({ loadError: errorMessage(e), ready: true });
    }
  },

  setPage: (page) => set({ page }),

  reloadInfo: async () => {
    try {
      set({ info: await api.appInfo() });
    } catch (e) {
      get().toast(errorMessage(e), "error");
    }
  },

  reloadAccounts: async () => {
    try {
      set({ accounts: await api.listAccounts() });
    } catch (e) {
      get().toast(errorMessage(e), "error");
    }
  },

  reloadStreams: async () => {
    try {
      set({ streams: await api.listStreams() });
    } catch (e) {
      get().toast(errorMessage(e), "error");
    }
  },

  updateSettings: async (patch) => {
    try {
      const settings = await api.updateSettings(patch);
      set({ settings });
      applyTheme(settings.theme);
    } catch (e) {
      get().toast(errorMessage(e), "error");
    }
  },

  toast: (text, kind = "info") => {
    const id = toastId++;
    set((s) => ({ toasts: [...s.toasts, { id, kind, text }] }));
    setTimeout(() => get().dismissToast(id), kind === "error" ? 9000 : 4500);
  },

  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
  openComposer: (seed) => set({ composer: seed ?? {} }),
  closeComposer: () => set({ composer: null }),
  openConnect: (platform) => set({ connectOpen: platform ?? true }),
  closeConnect: () => set({ connectOpen: null }),
  setUnread: (unread) => set({ unread }),
}));

export function platformInfo(id: Platform): PlatformInfo | undefined {
  return useApp.getState().info?.platforms.find((p) => p.id === id);
}

export function usePlatform(id: Platform | undefined): PlatformInfo | undefined {
  return useApp((s) => s.info?.platforms.find((p) => p.id === id));
}

export function useAccount(id: string | undefined): Account | undefined {
  return useApp((s) => s.accounts.find((a) => a.id === id));
}

export function replyRef(item: FeedItem, account: Account): ReplyRef {
  return {
    accountId: account.id,
    target: item.target,
    authorHandle: item.author.handle,
    textPreview: item.text.slice(0, 280),
    url: item.url,
  };
}

let mediaQuery: MediaQueryList | null = null;

export function applyTheme(theme: Settings["theme"]) {
  const root = document.documentElement;
  const resolve = () => {
    const dark = theme === "dark" || (theme === "system" && !!mediaQuery?.matches);
    root.dataset.theme = dark ? "dark" : "light";
  };
  if (!mediaQuery) {
    mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
  }
  mediaQuery.onchange = resolve;
  resolve();
}
