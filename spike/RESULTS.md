# Spike results: alacritty_terminal + glyphon/wgpu

Date: 2026-07-09 (local Oct 29)
Branch: rust_rewrite

## Question

Can a Rust terminal using `alacritty_terminal` (emulation + pty) rendered
with `glyphon` (cosmic_text) on `wgpu` handle agentic-style full-screen
redraw storms far better than VTE (the current mypixels backend)?

## Test

`stress.py <seconds> <fps> <statfile>` inside each terminal:

- alternate screen, 100x30 grid, full repaint every frame
- SGR 256-color per row, cursor reset, ~10 KB/frame
- paced to target fps; pty backpressure would show up as delivered < target

CPU = process CPU time delta over a 5s window mid-test (all threads),
one core = 100%. Windows were tiled full-screen (3838x2134) by the WM
for all terminals. AMD GPU, radv Vulkan, debug vs release builds noted.

## Results (avg CPU, % of one core)

| terminal                        | 30 fps   | 60 fps   | 120 fps  |
|---------------------------------|----------|----------|----------|
| VTE (mypixels, python)          | 57.5     | -        | 80.2     |
| alacritty 0.17 (GL, release)    | 4.6      | -        | -        |
| ghostty 1.3.1 (GPU, release)    | 10.6     | -        | -        |
| spike (alacritty_terminal + glyphon/wgpu) debug   | 22.2 | 43.6 | 83.4 |
| spike (alacritty_terminal + glyphon/wgpu) release | **2.8** | **5.0** | **9.8** |

All delivered the full target fps (300/600/1200 frames) — no pty
backpressure, no dropped frames on the emulation side.

## Notes

- The spike release build beats VTE ~20x at 30fps and ~8x at 120fps,
  while rendering a full-screen tiled window (more surface to clear than
  a 100x30 window).
- Debug build numbers are ~8x worse than release; expect the final
  (release) app to sit near the release column.
- Ghostty was surprisingly expensive here (10.6%); alacritty 4.6%.
- VTE's cost scales with fps (57% -> 80%), i.e. it burns CPU to merely
  keep up — exactly the chug seen with agentic workloads.

## Bugs found and fixed in the spike (relevant to the real port)

1. `PresentMode::Fifo` + blocking `get_current_texture` on radv/GNOME:
   when the compositor is busy, acquires time out for ~1s each and the
   screen freezes. Use `Mailbox` + treat `Suboptimal` as usable
   (present it) + `request_redraw` on `Timeout`.
2. Window retiling after creation: reconfigure surface + resize terminal
   in the `Resized` handler, not lazily in draw. (Residual gray strip
   after retiling — cosmetic, not chased.)
3. winit 0.30: key modifiers arrive via `WindowEvent::ModifiersChanged`,
   not on the key event.
4. glyphon pool of cosmic_text Buffers: per-run `&mut pool[i]` with
   long-lived shared refs needs a documented raw-pointer cast
   (disjoint indices, stable vec).
5. Custom rect vertex shader used pixel coords straight as clip space
   (`vec4(pos,0,1)`). A rect at pixel (0,0) lands at clip center, so the
   selected-tab background flooded the whole visible clip quadrant (a huge
   tan block). Fix: convert pixel->clip in the CPU, flipping y (clip +y is
   up, screen y is down): `cx=(px/w)*2-1, cy=1-(py/h)*2`.
6. Surface format was `Bgra8UnormSrgb`; wgpu re-encoded our already-sRGB
   colors as linear (double conversion, washed-out colors: #505020 showed
   as (152,152,99) instead of (80,80,32)). Fix: use non-sRGB
   `Bgra8Unorm` so display-referred colors write as-is.
7. glyphon's `TextAtlas::new` defaults to `ColorMode::Accurate`, which runs
   an sRGB->linear conversion on the *text* color in its shader. Against our
   as-is non-sRGB surface that made text too dark (badwolf red #ff2c4b
   rendered as (255,6,18), its linear encoding, while the matching rect
   background rendered correctly as (255,44,75)). Fix: build the atlas with
   `TextAtlas::with_color_mode(.., ColorMode::Web)` so the text color passes
   through unchanged, matching the rect pipeline and our display-referred
   sRGB colors. (Note: SGR 30-37/90-97 map to `Color::Named(..)`, not
   `Indexed(..)`; the 16-color palette override had to apply to the Named
   branch too, not just Indexed.)

## Offscreen visual testing (no desktop interference)

Run the spike under an Xvfb X server and capture the root with ImageMagick
`import`. No Wayland compositor, no focus stealing, fully scriptable.

```sh
Xvfb :99 -screen 0 1280x720x24 &
mkfifo /tmp/spikekeys
env -u WAYLAND_DISPLAY DISPLAY=:99 SPIKE_KEYS=/tmp/spikekeys \
  ./target/debug/spike sh &
DISPLAY=:99 import -window root /tmp/cap.png
```

`SPIKE_KEYS` FIFO feeds key specs (and `spy` to dump state). Guard each
write with `timeout 3 sh -c '... > fifo'` in case the reader is down.

## Verdict

GPU text rendering + alacritty_terminal emulation handles the agentic
redraw storm at single-digit CPU. The port is validated.

## Ported so far (beyond the rendering spike)

- `config.rs` — loads `~/.mypixels` (YAML, `!!python/tuple` + duplicate-key
  tolerant) into a `Config` with all visual/behavior defaults from the Python
  version; keybinding parse ("MOD1+SHIFT+Left", "CONTROL+ALT+x", "MOD1+_1",
  "SHIFT+KP_Add") and first-match `find_action` dispatch.
- `main.rs` — real fonts from config (term + tablist, pt->px at 96dpi),
  default window size from config (clamped to monitor), 16-color palette from
  config (Named+Indexed branches), config fg/bg/cursor, all tab colors.
- `tab.rs` — per-tab scrollback from config `scrollback_lines`.
- Live `config_reload` (Ctrl+Shift+R): re-reads the file and re-applies what
  can be re-applied. Colors/palette/tab colors/bindings are read at draw time
  (so swapping the struct covers them); a font change rebuilds the glyph pools
  + re-measures the cell + resizes the grid. Verified on Xvfb: changing
  term_font (Terminus->Cascadia) and background_color live re-rendered
  correctly and the shell kept working. `$SPIKE_CONFIG` overrides the config
  path for testing.
- Font size (enlarge/shrink, `zoom_term`): 1pt steps, unbounded up, floor of
  1pt down (matches the Python zoom()). Mutates the live `term_pt`, rebuilds
  the term glyph pool + re-measures the cell + resizes the grid. Config
  reload resets `term_pt` to the config value. Verified on Xvfb: 9->12 over
  three enlarges, then down to the 1pt floor (no 0pt).
- `rename_tab` (Alt+Shift+n): inline prompt on terminal row 0 (no GTK dialog).
  Draft prefills with the current title; chars/Backspace edit, Enter commits
  to `Tab.name` (which `tab_label` prefers over the window title), Esc
  cancels. While renaming, row-0 terminal runs + cursor are suppressed so the
  prompt owns the line, and no keys reach the terminal. Verified on Xvfb:
  shell->MyTab committed, and a second rename cancelled with Esc.

Not yet ported: search panel (deprioritized), tab overview (thumbnails).
Scrollback view, selection, IME, copy/paste still not implemented (spike is
a rendering/emulation perf + config test, not a full terminal yet).

## Remaining validation (manual)

Run a real agentic session inside `target/release/spike` and confirm the
feel under load on the real (non-Xvfb) GPU.

## Files

- `spike/src/main.rs` — app, rendering, key dispatch (winit 0.30 + wgpu 30
  + glyphon 0.12 + alacritty_terminal 0.26)
- `spike/src/config.rs` — `~/.mypixels` loading + keybindings
- `spike/src/tab.rs` — pty + alacritty Terminal per tab
- `spike/src/gpu.rs` — wgpu surface, rect + glyphon text pipelines
- `spike/stress.py` — the stress test
- `spike/cpusample.sh` — CPU sampler (not used in final protocol)
