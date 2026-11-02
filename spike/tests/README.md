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

For an unpaced stream of changing paths (without filesystem traversal), launch:

```sh
SPIKE_TRACE=1 spike/target/release/spike \
  python3 spike/tests/output_flood.py 10 /tmp/mypixels-flood.json
```

Use the same window size, compositor and GPU for comparisons. `SPIKE_TRACE`
reports cumulative frame-build and text-preparation times plus total frame time;
subtract build from text time to isolate layout. The JSON records lines produced
and elapsed time. A software renderer's CPU use is not comparable to a desktop
GPU's, and unpaced input tests throughput as well as rendering.

For preview rendering without xdotool, run from `spike/`:

```sh
xvfb-run -a python3 tests/preview_smoke.py
```

Requires ImageMagick `import` and Pillow. It compares a switcher thumbnail
against a downscaled live frame, checking all four edges, aspect ratio, glyph
placement, and colors. Screenshots are left in `/tmp/mypixels-preview-*`.
