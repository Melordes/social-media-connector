# Social Media Connector

A desktop app (Tauri 2 + React) for managing several social media accounts in one
place, in the spirit of Hootsuite: connect accounts, follow their timelines and
notifications side by side, write a post once and publish or schedule it to many
accounts, and keep an eye on what went out and how it did.

## What it does

- **Accounts** – connect as many accounts as you like, several per network. Sign-ins
  are stored in the system keyring (Secret Service: KWallet or GNOME Keyring), or, if
  there is none, in a file only your user can read.
- **Streams** – columns of timelines, notifications, mentions, hashtags, searches,
  subreddits… with like, repost, reply and delete, auto-refresh and paging.
- **Composer** – pick accounts, write once or customize the text per account, attach
  images with descriptions (alt text), set content warnings, visibility, language,
  Reddit title/subreddit, Tumblr tags and more. Live character counts use each
  network's own rules (links count 23 on Mastodon and X, Bluesky shortens long links,
  X weighs CJK and emoji double). Images are resized/converted to each network's limits
  and stripped of camera and location metadata.
- **Planner** – month calendar and lists of scheduled posts, drafts, sent posts and
  posts that need attention; edit, reschedule, publish now, retry failed accounts,
  duplicate, or delete a published post from a network.
- **Scheduling** – the app publishes scheduled posts itself, so it keeps running in the
  tray when you close the window (and can start at login). If it was not running at
  the scheduled time, a post can still go out late within a limit you choose;
  otherwise it waits in the Planner as *missed*.
- **Inbox** – notifications from all accounts in one list, with filters and unread
  markers.
- **Analytics** – posts sent per day, follower counts over time (recorded daily), and
  the best-performing recent posts per account.

## Supported networks

| Network | Sign-in | Read streams | Post | Notes |
|---|---|---|---|---|
| Mastodon (and GoToSocial, Akkoma, Pleroma…) | Browser, no setup | yes | text, images, CW, visibility | the app registers itself on your server |
| Bluesky / AT Protocol | App password | yes | text, images, link cards | create an app password in Bluesky's settings |
| Misskey (Sharkey, Firefish…) | Browser (MiAuth), no setup | yes | text, images, CW, visibility | |
| Telegram channels/groups | Bot token | – | text, images | add your bot as an admin that may post |
| Discord channels | Webhook URL | – | text, images | |
| X | Browser, **your X developer app** | home, mentions, own posts | text, images, reposts | X bills API use per request (credits from console.x.com); it no longer allows likes for self-serve apps, and replies only to posts that mention or quote you |
| Facebook Pages | Browser, **your Meta developer app** | Page posts | text, link, images, comments | one account per Page; the app stays in Development mode, no App Review needed for your own Pages |
| LinkedIn (personal profile) | Browser, **your LinkedIn developer app** | – | text, images | sign-ins last 60 days |
| Reddit | Browser, **your Reddit app** | yes | text and link posts, comments | needs an app with approved API access; Reddit is closing its public API (unregistered apps lose access from 12 Jan 2027, all public access ends by March 2027) |
| Tumblr | Browser, **your Tumblr app** | yes | text, images, links, tags, reblogs | posts go to your primary blog |

X, Facebook, LinkedIn, Reddit and Tumblr only accept apps registered by their users.
The app walks you through it: **Settings → Developer apps** (or the connect dialog)
shows the exact redirect URI to register, the steps, and a link to each developer site.
Redirect URIs look like `http://127.0.0.1:38917/callback` (or `localhost` for
Facebook and LinkedIn); the port can be changed in **Settings → Advanced**.

Instagram is not included: Meta's Instagram publishing API only takes images from a
public web address (it downloads them from your server), which a desktop app without its
own server cannot provide.

## Running and building

Requirements (Arch/CachyOS package names): `webkit2gtk-4.1`, `libsoup3`,
`libayatana-appindicator`, `librsvg`, `openssl`, `base-devel`, plus Rust (stable) and
Node.js 20+.

```sh
npm install
npm run tauri dev            # development, with hot reload
npm run tauri build          # release build and Linux packages (deb, rpm, AppImage)
```

The data (database, imported images, and the secrets file if no keyring is available)
lives in `~/.local/share/com.socialmediaconnector.desktop/`. Set `SMC_DATA_DIR` to use
another folder, and `SMC_SECRET_BACKEND=file` to skip the system keyring.

### Tests

```sh
cd src-tauri/core
cargo test                       # unit and mock-server tests for storage, scheduling and every network
cargo test -- --ignored live_    # read-only checks against public Bluesky and Mastodon data
```

## Project layout

```
src/                     React interface
  pages/                 Streams, Planner, Inbox, Analytics, Accounts, Settings
  components/            Composer, connect dialog, feed items, shared widgets
  lib/                   typed command wrappers, state, text counting, sanitizing
src-tauri/
  src/                   Tauri shell: commands, tray, window, plugins
  core/                  everything else, independent of Tauri:
    src/service.rs       accounts, sign-in flows, publishing, scheduler, inbox, analytics
    src/providers/       one module per network, plus their descriptions (limits, features)
    src/store.rs         SQLite storage
    src/secrets.rs       keyring / protected file
    src/oauth.rs         PKCE and the loopback redirect server
    src/media.rs         image import, resizing, metadata stripping
    src/text.rs          per-network character counting, link and mention detection
```

## Privacy and security notes

- Tokens never reach the interface; it only sees account ids.
- Post content from networks is sanitized before display, links open in your browser,
  and the window runs with a strict content security policy.
- Bot tokens and webhook URLs are kept out of error messages and logs.
- Removing an account deletes its stored sign-in; revoke the app's access on the
  network too if you no longer use it.
