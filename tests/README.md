# Regression checks

Run `cargo test --offline` from the repo root for font layout, configuration, clipboard
transfer bounds, and PTY paste encoding checks.

For keyboard/mouse behavior, build the debug binary, then run from the repository
root:

```sh
xvfb-run -a python3 tests/ui_smoke.py
```

Requires Xvfb, xdotool, xsel, and ImageMagick's `import`. The test creates an
isolated HOME and terminal child, checks the actual bytes pasted into the PTY,
and leaves its log and screenshot in the printed `/tmp/mypixels-ui-*` directory.
Clipboard reads are deliberately delayed to check that changing tabs during a
paste does not change its destination. Never point this test at your desktop.

To exercise native Wayland ownership, start a disposable compositor, then run
with its **isolated** runtime directory and display:

```sh
MYPIXELS_ISOLATED_CLIPBOARD_TEST=1 \
XDG_RUNTIME_DIR=/path/to/isolated/runtime WAYLAND_DISPLAY=wayland-1 \
cargo test --offline \
  wayland_interoperability -- --ignored --nocapture
```

Requires data-control support and wl-copy/wl-paste. It checks against independent
clipboard clients, including Unicode, trailing newlines, primary selection,
ownership changes, and repeated replacement of native sources. The explicit
test flag prevents accidentally running it against a normal desktop.

For an unpaced stream of changing paths (without filesystem traversal), launch:

```sh
MYPIXELS_TRACE=1 target/release/mypixels \
  python3 tests/output_flood.py 10 /tmp/mypixels-flood.json
```

Use the same window size, compositor and GPU for comparisons. `MYPIXELS_TRACE`
reports cumulative frame-build and text-preparation times plus total frame time;
subtract build from text time to isolate layout. The JSON records lines produced
and elapsed time. A software renderer's CPU use is not comparable to a desktop
GPU's, and unpaced input tests throughput as well as rendering.

For preview rendering without xdotool, run from the repo root:

```sh
xvfb-run -a python3 tests/preview_smoke.py
```

Requires ImageMagick `import` and Pillow. It compares a switcher thumbnail
against a downscaled live frame, checking all four edges, aspect ratio, glyph
placement, and colors. Screenshots are left in `/tmp/mypixels-preview-*`.

To check command history with real zsh, bash, and fish shells (whichever
are installed), run:

```sh
xvfb-run -a python3 tests/history_smoke.py
```

It uses isolated homes and multiline Unicode prompts, checks captured output
and re-running a command, and opens/dismisses the terminal context menu while
the history sidebar remains open. It requires xdotool, xsel, ImageMagick's
`import`, and Pillow, and leaves screenshots in `/tmp/mypixels-history-*`.

For settings navigation, Save/Cancel, font and color pickers, and a small
window preview:

```sh
xvfb-run -a python3 tests/settings_smoke.py
```

It uses an isolated HOME and configuration, checks persistence and cancellation,
and leaves screenshots in `/tmp/mypixels-settings-*`. Requires xdotool,
ImageMagick's `import`, and Pillow.

`xvfb-run -a python3 tests/switcher_smoke.py` exercises eight live terminal
previews, Tab/Shift-Tab and Enter, Escape restoration, card clicks, and navigation
in a small window. It saves screenshots for visual review. The settings smoke
also verifies exclusive sidebar visibility and identical surface backgrounds.

`xvfb-run -a python3 tests/panel_keyboard_smoke.py` checks that history and
settings remain passive for Tab, typing, Enter, Escape, and arrow keys, including
after clicking panel buttons. It also checks explicit editor focus and global
shortcuts. Set `MYPIXELS_BIN=target/release/mypixels` to check the release build.

`xvfb-run -a python3 tests/tab_bar_smoke.py` checks the egui strip's add,
selection, close and close-cancel behavior, drag reordering, 20-tab overflow,
middle-click close, and native detach into another window. It leaves visual
artifacts in `/tmp/mypixels-tab-bar-*`.
