#!/usr/bin/env node
// Writes src/generated/THIRD-PARTY-NOTICES.txt: the copyright notices and
// license texts of every third-party component that ends up in Social Media
// Connector's programs, as those components' licenses require:
//
// - the Rust crates compiled into the Linux and Windows programs (only
//   crates reached through normal dependencies, so build tools are left out),
// - the JavaScript packages bundled into the interface, and the helpers the
//   bundler adds to it,
// - native code that some crates carry (Microsoft's WebView2 loader).
//
// It runs before every build (`npm run build`, `npm run dev`), so the notices
// shown in the app and attached to each release always match its components.
// Texts for components that ship without a license file are kept in
// packaging/notices/ (see the README there).
//
// Usage: node scripts/third-party-notices.mjs [--out <file>] [--title <line>]
//   --title replaces "Social Media Connector <version>" under the heading.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const NOTICES = path.join(ROOT, "packaging", "notices");
const outArg = process.argv.indexOf("--out");
const OUT = outArg > 0 ? path.resolve(process.argv[outArg + 1]) : path.join(ROOT, "src", "generated", "THIRD-PARTY-NOTICES.txt");
const titleArg = process.argv.indexOf("--title");
const TITLE = titleArg > 0 ? process.argv[titleArg + 1] : null;

const TARGETS = [
  ["x86_64-unknown-linux-gnu", "Linux"],
  ["x86_64-pc-windows-msvc", "Windows"],
];

// Upstream license texts for components that ship without one, by name.
const CLARIFIED = {
  "alloc-stdlib": "alloc-stdlib.txt",
  "libappindicator-sys": "libappindicator-sys.txt",
  dlopen2: "dlopen2.txt",
  dlopen2_derive: "dlopen2.txt",
  "webview2-com": "webview2-com.txt",
  "webview2-com-sys": "webview2-com.txt",
  "webview2-com-macros": "webview2-com.txt",
  "@tauri-apps/plugin-dialog": "tauri-apps-plugin-dialog.txt",
};

const MIT_BODY = `Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`;

const warnings = [];

// ------------------------------------------------------------------ texts

const LICENSE_NAME = /^(licen[cs]e|copying|copyright|notice|unlicense)([-_. ].*)?$/i;
const NOT_A_TEXT = /\.(spdx|json|rs|toml|ya?ml|js|ts|html?)$/i;

function read(file) {
  return fs.readFileSync(file, "utf8").replace(/^﻿/, "");
}

/** Line endings unified, trailing spaces and surrounding blank lines removed. */
function normalize(text) {
  return text
    .replace(/\r\n?/g, "\n")
    .split("\n")
    .map((l) => l.replace(/\s+$/, ""))
    .join("\n")
    .replace(/\n{4,}/g, "\n\n\n")
    .replace(/^\n+|\n+$/g, "");
}

function licenseFiles(dir) {
  const files = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isFile() && LICENSE_NAME.test(entry.name) && !NOT_A_TEXT.test(entry.name)) files.push(full);
    if (entry.isDirectory() && /^licen[cs]es$/i.test(entry.name)) {
      for (const f of fs.readdirSync(full)) if (fs.statSync(path.join(full, f)).isFile()) files.push(path.join(full, f));
    }
  }
  return files.sort();
}

function classify(text) {
  const t = text.toLowerCase();
  if (t.includes("permission is hereby granted, free of charge")) return "MIT";
  if (t.includes("apache license") && t.includes("version 2.0")) return "Apache-2.0";
  if (t.includes("mozilla public license")) return "MPL-2.0";
  if (t.includes("free and unencumbered software released into the public domain")) return "Unlicense";
  if (t.includes("permission to use, copy, modify, and/or distribute this software for any")) return "ISC";
  if (t.includes("redistribution and use in source and binary forms")) return "BSD";
  if (t.includes("altered source versions must be plainly marked")) return "Zlib";
  return "other";
}

/**
 * The files to reproduce. For a plain choice of licenses ("MIT OR
 * Apache-2.0") one option is enough, preferably the shortest; anything else
 * gets every license file the component ships.
 */
function selectFiles(files, expression) {
  const notices = files.filter((f) => /^notice/i.test(path.basename(f)));
  const licenses = files.filter((f) => !notices.includes(f));
  const expr = (expression ?? "").trim().replace(/^\((.*)\)$/, "$1");
  if (expr && !/\bAND\b|[()]/.test(expr) && /\bOR\b|\//.test(expr)) {
    const options = expr.split(/\s+OR\s+|\s*\/\s*/).map((s) => s.trim());
    for (const want of ["MIT", "ISC", "BSD-3-Clause", "BSD-2-Clause", "Zlib", "Apache-2.0", "Unlicense", "MPL-2.0"]) {
      if (!options.includes(want)) continue;
      const kind = want.startsWith("BSD") ? "BSD" : want;
      const match = licenses.filter((f) => classify(read(f)) === kind);
      if (match.length) return [...match, ...notices];
    }
  }
  return files;
}

// -------------------------------------------------------------- components

function rustCrates() {
  const byId = new Map();
  for (const [triple, label] of TARGETS) {
    const json = execFileSync(
      "cargo",
      ["metadata", "--format-version", "1", "--manifest-path", path.join(ROOT, "src-tauri", "Cargo.toml"), "--filter-platform", triple],
      { cwd: ROOT, encoding: "utf8", maxBuffer: 512 * 1024 * 1024, stdio: ["ignore", "pipe", "inherit"] },
    );
    const meta = JSON.parse(json);
    const packages = new Map(meta.packages.map((p) => [p.id, p]));
    const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
    const seen = new Set();
    const stack = [meta.resolve.root];
    while (stack.length) {
      const id = stack.pop();
      if (seen.has(id)) continue;
      seen.add(id);
      for (const dep of nodes.get(id).deps) {
        // Normal dependencies only: build scripts' and tests' tools don't ship.
        if (dep.dep_kinds.some((k) => k.kind === null)) stack.push(dep.pkg);
      }
    }
    for (const id of seen) {
      const p = packages.get(id);
      if (!p.source) continue; // this project's own crates
      const entry = byId.get(id) ?? { p, platforms: new Set() };
      entry.platforms.add(label);
      byId.set(id, entry);
    }
  }
  return [...byId.values()].map(({ p, platforms }) => {
    const dir = path.dirname(p.manifest_path);
    const files = licenseFiles(dir);
    if (p.license_file) {
      const f = path.resolve(dir, p.license_file);
      if (fs.existsSync(f) && !files.includes(f)) files.push(f);
    }
    return {
      group: "rust",
      name: p.name,
      version: p.version,
      license: p.license ?? "see license text",
      authors: p.authors ?? [],
      files,
      source: `https://crates.io/crates/${p.name}/${p.version}`,
      platforms: [...platforms].sort(),
    };
  });
}

function resolvePackage(name, fromDir) {
  for (let dir = fromDir; ; dir = path.dirname(dir)) {
    const candidate = path.join(dir, "node_modules", name);
    if (fs.existsSync(path.join(candidate, "package.json"))) return candidate;
    if (path.dirname(dir) === dir) return null;
  }
}

function jsPackages() {
  const found = new Map();
  const visit = (name, fromDir, follow = true) => {
    const dir = resolvePackage(name, fromDir);
    if (!dir || found.has(dir)) return;
    const pkg = JSON.parse(read(path.join(dir, "package.json")));
    const typesOnly = name.startsWith("@types/") || (!pkg.main && !pkg.module && !pkg.exports && !pkg.browser && (pkg.types || pkg.typings));
    found.set(dir, typesOnly ? null : { pkg, dir });
    if (follow) for (const dep of Object.keys({ ...pkg.dependencies, ...pkg.optionalDependencies })) visit(dep, dir);
  };
  const root = JSON.parse(read(path.join(ROOT, "package.json")));
  for (const dep of Object.keys(root.dependencies ?? {})) visit(dep, ROOT);
  // The bundler adds small helpers of its own to the interface's code.
  visit("vite", ROOT, false);
  visit("rolldown", ROOT, false);
  return [...found.values()].filter(Boolean).map(({ pkg, dir }) => {
    let files = licenseFiles(dir);
    let text = null;
    if (pkg.name === "vite") {
      // Vite's LICENSE.md also lists the licenses of Vite's own bundled
      // dependencies, which stay in the build tool; keep Vite's own license.
      text = read(path.join(dir, "LICENSE.md")).split(/\n# Licenses of bundled dependencies/)[0].replace(/^# Vite core license\s*/, "");
      files = [];
    }
    const author = typeof pkg.author === "string" ? pkg.author : pkg.author?.name;
    return {
      group: pkg.name === "vite" || pkg.name === "rolldown" ? "bundler" : "js",
      name: pkg.name,
      version: pkg.version,
      license: typeof pkg.license === "string" ? pkg.license : (pkg.license?.type ?? "see license text"),
      authors: author ? [author] : [],
      files,
      text,
      source: `https://www.npmjs.com/package/${pkg.name}/v/${pkg.version}`,
      platforms: [],
    };
  });
}

// ------------------------------------------------------------------ output

function textsFor(c, representative) {
  if (c.text) return [normalize(c.text)];
  const selected = selectFiles(c.files, c.license);
  if (selected.length) return selected.map((f) => normalize(read(f)));
  if (CLARIFIED[c.name]) return [normalize(read(path.join(NOTICES, "clarified", CLARIFIED[c.name])))];
  const ids = c.license.replace(/[()]/g, " ").split(/\s+(?:OR|AND)\s+|\s*\/\s*/).map((s) => s.trim());
  if (ids.includes("MIT")) {
    const who = c.authors.map((a) => a.replace(/\s*<[^>]*>/, "")).join(", ") || `the ${c.name} authors`;
    return [`MIT License\n\nCopyright (c) ${who}\n\n${MIT_BODY}`];
  }
  for (const id of ids) {
    if (representative.has(id)) return [representative.get(id)];
  }
  warnings.push(`${c.name} ${c.version}: no license text found (${c.license})`);
  return [];
}

function wrap(words, width = 78, indent = "  ") {
  const lines = [];
  let line = indent;
  for (const w of words) {
    if (line.length + w.length + 2 > width && line.trim()) {
      lines.push(line.replace(/,?\s*$/, ","));
      line = indent;
    }
    line += (line.trim() ? ", " : "") + w;
  }
  if (line.trim()) lines.push(line);
  return lines.join("\n");
}

function main() {
  const app = JSON.parse(read(path.join(ROOT, "package.json")));
  const components = [...rustCrates(), ...jsPackages()].sort(
    (a, b) => a.group.localeCompare(b.group) || a.name.localeCompare(b.name) || a.version.localeCompare(b.version),
  );

  // Canonical texts for components that ship none, taken from components
  // under one license with a single license file (e.g. MPL-2.0).
  const representative = new Map();
  for (const c of components) {
    if (c.files.length === 1 && /^[A-Za-z0-9.-]+$/.test(c.license) && !representative.has(c.license)) {
      if (classify(read(c.files[0])) !== "MIT") representative.set(c.license, normalize(read(c.files[0])));
    }
  }

  const groups = new Map();
  for (const c of components) {
    const label = `${c.name} ${c.version}`;
    for (const text of textsFor(c, representative)) {
      const g = groups.get(text) ?? { text, users: [] };
      if (!g.users.includes(label)) g.users.push(label);
      groups.set(text, g);
    }
  }

  const extra = [
    {
      title: "Microsoft Edge WebView2 loader (Windows program only)",
      intro:
        "The Windows program contains Microsoft's WebView2 loader (WebView2LoaderStatic.lib from the\nMicrosoft.Web.WebView2 SDK, through the webview2-com-sys crate). Its license and notice:",
      texts: [read(path.join(NOTICES, "WebView2-SDK-LICENSE.txt")), read(path.join(NOTICES, "WebView2-SDK-NOTICE.txt"))].map(normalize),
    },
  ];

  const rule = "=".repeat(78);
  const thin = "-".repeat(78);
  const out = [];
  out.push(
    `THIRD-PARTY SOFTWARE NOTICES`,
    TITLE ?? `Social Media Connector ${app.version}`,
    "",
    `Social Media Connector itself is free and unencumbered software released into`,
    `the public domain (The Unlicense). It is built with the open-source components`,
    `listed below, which keep their own licenses. This file reproduces their`,
    `copyright notices and license texts, as those licenses require.`,
    "",
    `- Source code: each component's source is available at the address listed with`,
    `  it (Rust crates on crates.io, JavaScript packages on npm). For components`,
    `  under the Mozilla Public License, that is where their source code is offered.`,
    `- Where a component lets users choose between licenses, the license reproduced`,
    `  here is the one under which it is used.`,
    `- SQLite, compiled in through the libsqlite3-sys crate, is in the public domain.`,
    `- On Linux the program uses the system's own WebKitGTK and GTK libraries; they`,
    `  are not part of Social Media Connector's downloads.`,
    `- The Windows installers are made with NSIS (zlib license) and the WiX Toolset,`,
    `  and use Tauri's NSIS helper plugin, under the same terms as Tauri below.`,
    `- The networks' names and logos (drawn with Simple Icons, CC0-1.0) are`,
    `  trademarks of their owners and only identify the networks.`,
    "",
    rule,
    "COMPONENTS",
    rule,
  );
  const sections = [
    ["rust", "Rust crates compiled into the program"],
    ["js", "JavaScript packages bundled into the interface"],
    ["bundler", "Helpers added to the interface by the build tools"],
  ];
  for (const [group, title] of sections) {
    const list = components.filter((c) => c.group === group);
    out.push("", `${title} (${list.length}):`, "");
    for (const c of list) {
      const only = c.platforms.length === 1 ? ` [${c.platforms[0]} only]` : "";
      out.push(`${c.name} ${c.version}${only}`, `  License: ${c.license}`, `  Source: ${c.source}`);
    }
  }
  out.push("", "Native code:", "", "Microsoft Edge WebView2 loader [Windows only]", "  License: Microsoft WebView2 SDK license (BSD-style), reproduced at the end");

  out.push("", rule, "LICENSE TEXTS", rule);
  const sorted = [...groups.values()].sort((a, b) => a.users[0].localeCompare(b.users[0]));
  for (const g of sorted) {
    out.push("", thin, "Applies to:", wrap(g.users), thin, "", g.text);
  }
  for (const e of extra) {
    out.push("", thin, e.title, thin, "", e.intro, "", ...e.texts.flatMap((t) => [t, ""]));
  }

  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, out.join("\n").replace(/\n+$/, "") + "\n");

  const counts = sections.map(([g]) => components.filter((c) => c.group === g).length);
  console.log(
    `third-party notices: ${counts[0]} crates, ${counts[1]} packages, ${counts[2]} build helpers, ` +
      `${groups.size} distinct license texts -> ${path.relative(ROOT, OUT)} (${Math.round(fs.statSync(OUT).size / 1024)} KB)`,
  );
  for (const w of warnings) console.warn(`warning: ${w}`);
  if (warnings.length) process.exitCode = 1;
}

main();
