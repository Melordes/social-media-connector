Social Media Connector for Linux (portable archive)
===================================================

Run it from this folder:

    ./social-media-connector

It uses your system's WebKitGTK 4.1 and GTK 3. Most desktops already have
them; if the program doesn't start, install WebKitGTK 4.1:

    Arch, CachyOS, Manjaro:   sudo pacman -S webkit2gtk-4.1
    Debian, Ubuntu, Mint:     sudo apt install libwebkit2gtk-4.1-0

On Debian, Ubuntu, Fedora and openSUSE the .deb and .rpm packages from the
same release page install everything for you instead.

The tray icon needs libayatana-appindicator (Arch: libayatana-appindicator,
Debian and Ubuntu: libayatana-appindicator3-1). Without it the app still
works, but closing the window quits it instead of keeping it in the tray.

To add it to your application menu, run in this folder:

    install -Dm755 social-media-connector ~/.local/bin/social-media-connector
    install -Dm644 social-media-connector.png ~/.local/share/icons/hicolor/256x256/apps/social-media-connector.png
    install -Dm644 social-media-connector.desktop ~/.local/share/applications/social-media-connector.desktop

(~/.local/bin has to be on your PATH; it is on most distributions.)

Social Media Connector is free and unencumbered software in the public
domain: see LICENSE. The open-source components it is built with, and their
licenses, are listed in THIRD-PARTY-NOTICES.txt (also in the app, under
Settings, About).

Source code: https://github.com/Melordes/social-media-connector
