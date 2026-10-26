# mdread

A very fast, GPU-rendered Markdown viewer for Linux and macOS, built for large
high-DPI screens. Rust + [egui](https://github.com/emilk/egui) (OpenGL), parses with
pulldown-cmark, highlights code with syntect.

- Cold start to first frame in about 100 ms; an 800 KB / 1100-block document
  parses in ~4 ms and renders in under 2 ms per frame (only visible blocks are
  laid out).
- Crisp text at any scale, including fractional Wayland scaling and Retina,
  using the system's Ubuntu Sans or SF Pro (variable weight) and JetBrains Mono
  or any installed Nerd Font.
- Widescreen layout: outline sidebar that tracks the section you are reading,
  files sidebar for the rest of the folder, and a reading column capped at a
  comfortable measure (adjustable, or full width).
- Live reload when the file changes on disk.
- Dark and light themes, following the system by default.
- GFM: tables, task lists, strikethrough, footnotes, alerts (`> [!NOTE]`),
  definition lists, front matter, math (shown as monospace), local and remote
  images (PNG, JPEG, GIF, WebP, SVG), inline badges, HTML `<img>`.
- Links: web links open in the browser, relative `.md` links open in the
  viewer (with back/forward), `#anchors` jump within the document.
- Search with highlighted matches, copy button on code blocks, text selection,
  right-click menu to copy the selection or open and copy links.

## Build

```bash
cargo build --release          # binary at target/release/mdread
./install.sh                   # installs to ~/.local/bin, adds a desktop entry and
                               # icons, and makes mdread the default for Markdown files
```

Requires rustc 1.92+ (Ubuntu 26.04 ships 1.93 and cargo via `apt install cargo`).
No system GUI libraries are needed beyond Mesa/Wayland/X11 runtime libraries.
No packaging (.deb) is needed: a per-user desktop entry is enough for GNOME,
Nautilus and `xdg-open`. The installer writes the binary's absolute path into
the entry because GNOME launches apps with a `PATH` that does not include
`~/.local/bin`, and GLib silently drops entries whose `Exec` it cannot resolve.
To undo the default: `xdg-mime default org.gnome.TextEditor.desktop text/markdown`.
The installer also puts the app icon into `~/.local/share/icons/hicolor`, which
every icon theme falls back to, so the dock, window switcher and app grid show
mdread's own icon rather than a placeholder (icon sources and a render script
are in `assets/icons`). The icon is embedded in the binary too, which covers the
X11 window icon and the macOS Dock while mdread is running.

On macOS, `cargo build --release` works unchanged (install the toolchain with
`brew install rustup && rustup default stable`; the build needs Xcode Command
Line Tools and nothing else). `./install.sh` copies the binary to `~/.local/bin`
and skips the desktop entry. Shortcuts use `⌘` where the table below says `Ctrl`,
and the file dialog is the native panel instead of zenity.

## Usage

```bash
mdread README.md
mdread docs/guide.md#installation     # open at a heading
mdread notes.md --find TODO           # open with the search box filled in
cat file.md | mdread -                # from stdin
mdread --light file.md                # force a theme for this session
```

Open the file dialog with `Ctrl+O` (`⌘O` on macOS; uses zenity on Linux) or drop
a file onto the window.

## Keys

| Keys | Action |
|---|---|
| `j` `k` `↓` `↑` | scroll by line |
| `Space` `PgDn` / `Shift+Space` `PgUp` | scroll by page |
| `d` `u` | half page |
| `g` `G` `Home` `End` | top / bottom |
| `/` `Ctrl+F` | find; `Enter` `n` `N` next / previous |
| `o` | outline sidebar |
| `f` | files sidebar |
| `t` | dark / light |
| `w` | full-width column; `[` `]` narrower / wider |
| `Ctrl +` `Ctrl -` `Ctrl 0`, `Ctrl`+scroll | zoom |
| `Alt+←` `Backspace` / `Alt+→` | back / forward |
| `r` `F5` | reload |
| `?` `F1` | help |
| `q` `Ctrl+Q` | quit |

Settings (zoom, column width, sidebars, theme, window size) persist between runs.

## Fonts

Body text uses the first of: Ubuntu Sans (variable), Ubuntu, Noto Sans,
DejaVu Sans, SF Pro (macOS, variable), Arial. Code uses the first of: JetBrains
Mono, any installed [Nerd Font](https://www.nerdfonts.com/) (`*NerdFontMono-Regular.ttf`,
then `*NerdFont-Regular.ttf`), Ubuntu Sans Mono, Ubuntu Mono, Fira Code, DejaVu
Sans Mono, SF Mono, Monaco. Fonts are found by file name in the user font
directories (`~/.local/share/fonts`, `~/.fonts`, `~/Library/Fonts`) before the
system ones, so installing a preferred font per user is enough. Override with
`MDREAD_FONT=/path/to/font.ttf` and `MDREAD_MONO_FONT=...`.

## Debugging

```bash
RUST_LOG=mdread=debug mdread file.md            # parse timing, image errors
RUST_LOG=mdread=trace mdread file.md            # per-frame timing
mdread --screenshot out.png --screenshot-delay 1500 file.md   # headless-ish capture
```

## License

[MIT](LICENSE).
