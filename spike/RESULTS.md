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

## Remaining validation (manual)

Run a real agentic session inside `target/release/spike` and confirm the
feel (scrollback, selection, IME, copy/paste are NOT implemented in the
spike — it is a rendering/emulation perf test only).

## Files

- `spike/src/main.rs` — the spike (winit 0.30 + wgpu 30 + glyphon 0.12
  + alacritty_terminal 0.26)
- `spike/stress.py` — the stress test
- `spike/cpusample.sh` — CPU sampler (not used in final protocol)
