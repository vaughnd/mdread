#!/usr/bin/env bash
# Install mdread for the current user: ~/.local/bin/mdread + desktop entry,
# and register it as the default application for Markdown files.
set -euo pipefail
cd "$(dirname "$0")"
# Git Bash / MSYS: this script installs the XDG desktop integration, which Windows lacks.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    echo 'On Windows use install.ps1 (powershell -ExecutionPolicy Bypass -File .\install.ps1) or the setup program built from packaging/windows/mdread.iss.' >&2
    exit 1;;
esac
# Always build: cargo skips the work when nothing changed, and guarding on the
# binary's existence would install a stale build after source edits.
cargo build --release
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
BIN="$BIN_DIR/mdread"
mkdir -p "$BIN_DIR"
install -m 755 target/release/mdread "$BIN"
if [ "$(uname -s)" = "Darwin" ]; then
  # No desktop entry or MIME registration on macOS: mdread is run from a shell
  # (`mdread file.md`), via Cmd+O, or by dropping a file onto the window.
  echo "Installed $BIN."
  case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "Note: add $BIN_DIR to your PATH to run 'mdread' from a shell." ;; esac
  exit 0
fi
mkdir -p "$APP_DIR"
# The desktop entry must carry an absolute Exec path: GNOME's session PATH
# does not include ~/.local/bin, and GLib ignores entries it cannot resolve.
sed "s|@BIN@|$BIN|g" mdread.desktop > "$APP_DIR/mdread.desktop"
chmod 644 "$APP_DIR/mdread.desktop"
update-desktop-database "$APP_DIR" 2>/dev/null || true
# Icons: the desktop entry names "mdread", which GNOME looks up in the hicolor
# theme (the fallback every icon theme inherits). Without these it shows a
# placeholder in the dock, window switcher and app grid. Wayland compositors
# take the icon from the desktop entry matched by app id, not from the window.
ICON_DIR="$HOME/.local/share/icons/hicolor"
for png in assets/icons/hicolor/*/apps/mdread.png; do
  size=$(basename "$(dirname "$(dirname "$png")")")
  install -D -m 644 "$png" "$ICON_DIR/$size/apps/mdread.png"
done
install -D -m 644 assets/icons/mdread.svg "$ICON_DIR/scalable/apps/mdread.svg"
gtk-update-icon-cache -q -t -f "$ICON_DIR" 2>/dev/null || true
echo "Installed $BIN, $APP_DIR/mdread.desktop and icons under $ICON_DIR."
if command -v xdg-mime >/dev/null 2>&1; then
  xdg-mime default mdread.desktop text/markdown text/x-markdown
  echo "Registered mdread as the default application for Markdown files."
else
  echo "To make it the default viewer for Markdown files:"
  echo "  xdg-mime default mdread.desktop text/markdown"
fi
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "Note: $BIN_DIR is not on your PATH (needed only for running 'mdread' from a shell)." ;; esac
