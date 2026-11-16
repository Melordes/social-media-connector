// What the About section shows: where the source code lives, and the
// third-party license notices (generated before every build by
// scripts/third-party-notices.mjs, loaded only when someone opens them).

import pkg from "../../package.json";

export const REPOSITORY_URL = pkg.repository.url.replace(/^git\+/, "").replace(/\.git$/, "");

export function loadNotices(): Promise<string> {
  return import("../generated/THIRD-PARTY-NOTICES.txt?raw").then((m) => m.default);
}
