# License texts kept here

`scripts/third-party-notices.mjs` builds the third-party notices that ship with
the app (Settings → About → Open-source licenses, and `THIRD-PARTY-NOTICES.txt`
on every release). It reads each component's license files from its package.
The texts here fill the gaps:

- `clarified/` — components that ship without a license file in their package.
  Each text is the license file of the component's upstream repository:
  - `alloc-stdlib.txt`: github.com/dropbox/rust-alloc-no-stdlib, `LICENSE`
  - `dlopen2.txt` (dlopen2, dlopen2_derive): github.com/OpenByteDev/dlopen2, `LICENSE`
  - `libappindicator-sys.txt`: github.com/tauri-apps/libappindicator-rs, `LICENSE-MIT`
  - `webview2-com.txt` (webview2-com, -sys, -macros): github.com/wravery/webview2-rs, `LICENSE`
  - `tauri-apps-plugin-dialog.txt`: github.com/tauri-apps/plugins-workspace,
    `plugins/dialog/LICENSE_MIT`
- `WebView2-SDK-LICENSE.txt` and `WebView2-SDK-NOTICE.txt` — `LICENSE.txt` and
  `NOTICE.txt` of the Microsoft.Web.WebView2 1.0.3650.58 NuGet package. The
  Windows program links Microsoft's WebView2 loader from that SDK (through the
  webview2-com-sys crate), and the license asks binary distributions to
  reproduce it.

When a dependency is added or updated, run `npm run notices`: it warns about
any component whose license text it can't find, so a text can be added here.
