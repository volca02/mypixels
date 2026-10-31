# Regression checks

Run `cargo test --offline` from `spike/` for font layout, configuration, clipboard
transfer bounds, and PTY paste encoding checks.

For keyboard/mouse behavior, build the debug binary, then run from the repository
root:

```sh
xvfb-run -a python3 spike/tests/ui_smoke.py
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
cargo test --offline --manifest-path spike/Cargo.toml \
  wayland_interoperability -- --ignored --nocapture
```

Requires data-control support and wl-copy/wl-paste. It checks against independent
clipboard clients, including Unicode, trailing newlines, primary selection,
ownership changes, and repeated replacement of native sources. The explicit
test flag prevents accidentally running it against a normal desktop.
