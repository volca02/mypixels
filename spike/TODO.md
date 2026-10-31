# Rust rewrite priorities

Scope: preserve the **Alt-held visual tab switcher**. The unused standalone
`Alt+O` overview is retired; legacy overview bindings are ignored.

Every feature should pass three gates: correct terminal behavior, responsive
interaction under output load, and a visual check with the configured fonts.
Performance measurements below use release builds unless marked otherwise.

Current priorities:

1. **Readable and responsive terminal (P0).** Fixed filename wrapping/overlap,
   fractional grid drift, ignored bold/italic, non-ASCII fallback replacing a
   font's own glyphs, invisible padding being shaped, and unpaced Wayland
   redraws. Keep `ls -la`, mixed Unicode, and sustained scrolling as regressions.
2. **Clipboard and selection (P0, completed).** Native Wayland plus compatibility
   paths, asynchronous paste with stable destinations, separate primary,
   bracketed paste, scrollback, and real keyboard/mouse tests. Details below.
3. **Everyday interaction (P1).** Proper editable rename input with selected
   initial text and clipboard support; IME; terminal mouse reporting with Shift
   bypass; selection autoscroll at the viewport edge; HiDPI transitions.
4. **Tab strip usability (P1).** Keep the active tab reachable with many tabs,
   implement configured wrapping/placement, and verify full title updates and
   consistent hover/focus states. Preserve the compact layout and familiar keys.
5. **Search (P2).** A reusable inline text input, incremental search, next/previous
   matches, visible match state, and cancellation back to the terminal.

Remaining investigations and historical notes follow.

# Spike polish notes

Look/feel gaps against the original (Python/GTK/VTE) mypixels. Done items keep
their notes: the causes were rarely where the symptom pointed.

## Done

- [x] **Prompt had extra square characters.** Not a bad font choice -- a missing
  fallback. starship draws Nerd Font PUA codepoints (`U+E0B0` powerline, `U+E70E`
  branch, `U+F300` logos) that plain Cascadia Code does not contain; VTE/Pango get
  them because fontconfig substitutes "Cascadia Code NF". cosmic-text 0.19 instead
  takes a single fallback shot at the generic monospace family, which it seeds with
  "Noto Sans Mono" (no PUA), so every one became tofu. Fixed in `src/fonts.rs`.
- [x] **Bar too tall.** Line height is now read from the font (`upem`, `ascent`,
  `descent`, `leading`, each rounded separately, Pango-style) instead of the
  invented `font_px * 1.3`. The strip lands on 21px, exactly what the original
  measures, down from 27px. Two traps: skrifa reports `descent` *negative*, and
  Terminus carries leading that must be counted.
- [x] **Bare-bones styling.** `tablist_background` bar, 8px/2px GTK padding, 1px
  gaps, bracketed `[title]` labels, mauve `HH:MM` clock prepend, `[+]` on
  `brighten(tablist_background, 1.1)` -- plus a 2px accent under the active tab in
  the tab's own colour, which keeps selection legible when fg and bg barely differ.
- [x] **Not clickable.** Hover + click to select, click `[+]` to add. Hit boxes are
  recorded while drawing, so hit-testing cannot drift from the pixels.
- [x] **Cursor does not blink.** Activity-aware: solid while the terminal is
  producing output or right after a keypress, ~533ms on/off when idle. Blocked by a
  systemic bug, not a missing feature -- see bug 9 in RESULTS.md.
- [x] **Overview/switcher looked like grey mush.** Previews no longer shrink the
  whole grid (170 columns into 300px is unreadable); they keep glyphs legible and
  crop, anchored on the cursor, clipped to the tile. Tiles are rounded with stacked
  shadow rects and an index chip in the tab's group colour.

## Still open

- [ ] **See-through switcher (half done).** The terminal is now painted under the
      overlay, so the dim backdrop means something and coloured backgrounds show
      through. Terminal *text* does not, and cannot with the current single render
      pass: glyphon draws all text after all rects, so live text bled through the
      tiles. Behind the overview the terminal is therefore drawn as a colour field
      only. Real transparency needs the overlay in its own render pass (`LoadOp::Load`
      for rects) *and* a second `TextRenderer` -- calling `TextRenderer::prepare`
      twice a frame with the same renderer swaps the vertex buffers its earlier
      draw already referenced, so one renderer cannot serve two passes.
- [x] **Output-flood layout cost.** Reproduced with changing paths on a 4K
      viewport. Trim invisible row padding, use basic shaping for ASCII, and
      notify Wayland before presenting so the compositor can pace frames. See
      RESULTS.md for the measured improvement.
- [ ] **Other debug-build CPU.** Earlier measurements were not reproducible offscreen: debug idles at 2 fps /
      4.2% CPU on Xvfb, and coverage resolution is cheap (11 tests including 7
      codepoint scans run in 0.43s unoptimised). Needs a measurement from the real
      Wayland session. The correct fix is damage-driven rendering -- `Term::damage()`
      is public and reports dirty rows, but our glyph pool is indexed by run, so
      per-row caching needs a slot-per-row layout first.

- [ ] **Titles don't live-update.** They track the terminal title via `Tab.title`,
      but confirm the strip re-renders on change at the original's cadence, and that
      the rename prompt seeds correctly (renaming a tab titled `shell` produced
      `shellworktree` -- the draft appears to append rather than replace).
- [ ] **Tablist font is not the original's metrics.** The family resolves, but the
      strip's advance width still reads slightly different from GTK's Terminus.
- [x] Terminal cell height now matches the measured Pango row (15px for
      Cascadia Code 9pt). The hhea ascent/descent each round upward; regression
      checks cover the configured terminal and tab fonts.
- [ ] Search panel (deprioritised).
- [x] Scrollback view, selection, and copy/paste. Mouse wheel, Shift+PageUp/Down,
      and Ctrl+Shift+End; terminal-owned selection tracks output. IME is still open.

## Notes

- Keep the Xvfb + `import` capture loop for visual checks (see RESULTS.md). Capture
  by window id (`xdotool search --name`), not root: the reference app and the spike
  overlap on the same display.
- The reference app runs offscreen with `HOME=/tmp/pyhome`, whose `.zshrc` prints a
  ruler; that is how the 7x15 cell and 21px strip were measured.

## Switcher previews: real downscaled frames (done), one caveat

Shipped: every tile samples a real render. Each tab's screen is shaped into a
`Buffer` of its own -- one line per terminal row, gaps padded with spaces so
columns land on the grid -- drawn into an offscreen texture at window size, and
sampled back into the tile by a new pipeline with a 4x4 box filter. Nothing is
shaped every frame: a preview is rebuilt when its content changed *and* its own
throttle expired (60ms for the selected tile, 200ms for the rest), at most two
per frame, so a busy grid cannot turn one redraw into a 60ms hitch. A throttled
tile keeps showing its previous frame, which is what a thumbnail needs anyway.

The colour-block previews are gone. Mixed styles read as broken: half the grid
looking like a picture and half like a bar chart looks like a bug even when each
half is working.

Two bugs found by looking at the output rather than at the code:

- Binding the whole quad vertex buffer and drawing `0..6` per tile painted the
  *first* tile's geometry once per texture. Only one tile showed anything, and it
  showed the wrong tab. Slice the buffer per quad.
- A round-robin that seeded with the selected tab and rotated from index 1 could
  list one tab twice and starve another forever.

What is left:

- Dense small text reads faint. A faithful downscale of thin grey glyphs on black
  is mostly black, so a wall of output looks dimmer than the coloured prompt
  lines. If that is annoying, lift it in the sampling shader (`PREV_FS_WGSL`),
  not in the offscreen render.
- The same pass now makes a true see-through switcher possible: the offscreen
  target already holds a complete frame per tab.

Old notes, kept because the causes were not obvious:

`draw_overview` paints one tinted block per non-blank run (the run's own
foreground at 0.62 alpha). It reads as "where is the text" and nothing else. The
terminal grid is the resolution ceiling, so it looks like a bitmap of cells
rather than a small window.

Real fix: render the tab's frame at normal scale into an offscreen texture and
sample that into the tile. A first attempt stalled on two things that are not
obvious from the description, so they are written down here.

**What is easy.** glyphon 0.12 takes the render pass, not the target:
`TextRenderer::render(&atlas, &viewport, &mut pass)`. One `TextRenderer` and one
atlas can draw into an offscreen pass as long as the texture format matches ours
(`Bgra8Unorm`); the only extra object is a second `Cache` + `Viewport` at the
offscreen size. Glyphs are already in the atlas, so a second pass costs
positioning, not rasterisation. Sampling needs a new pipeline: quad vertices
carrying `pos`, `uv`, and a box half-width in UV (one destination pixel covers
`1/tile_w` of the texture), and a 4x4 box filter in the fragment stage -- plain
bilinear samples one texel in eight at this ratio and aliases worse than the
blocks it replaces. Quads go between the rect pass and the text pass so tile
headers stay on top. Sample `uv` 0..1 over the whole window: the tab strip comes
with it, which makes the tile a picture of the window, and fitting the window
rect into the thumb with the existing `scale` keeps the aspect ratio.
Clear the offscreen to *opaque* black: the window background rect carries alpha
`~0.82`, so a translucent thumbnail composites to mud; draw the tile quad with
no blending.

**Landmine 1: there is nothing free to reuse.** While the overview is open the
active tab's runs are deliberately never shaped (`if self.overview.is_some() {
break }` in the text loop), because glyphon draws all text after all rects and
terminal text would bleed through the tiles. So the offscreen pass cannot share
the main pass's areas -- it shapes its own. That is cheaper than it sounds once
the padding is trimmed (below): about 4ms for a 545x150 screen in release, and
the overview frame as a whole costs 7ms against 30ms for a plain frame, because
the terminal's own text is suppressed while the overlay is up.

**Landmine 2: `gpu.pool` is not free either.** `draw_overview` takes slots from
the same pool for tile headers and index chips, in the same frame, so preview
areas cannot use those slots. Solved by the single-buffer route: `prev_bufs` is
one `Buffer` per tab, holding a whole screen as `set_rich_text` spans (`\n` inside
a span starts a new line), with `set_metrics(cell_w, cell_h)` so rows advance one
cell. It needs one buffer per preview instead of thousands of shared slots, and
`frame_sig` means an unchanged screen is redrawn but never re-shaped.

**Landmine 3, found while measuring: end-of-line padding.** Runs carry their
trailing spaces out to the end of the line. Invisible, and fatal: 545 of them per
row is 80k glyphs a refresh, and the overview took seconds to draw one frame.
`preview_spans` trims them and drops all-padding runs, which is most of the
difference between unusable and 4ms.

The same pass also closes the see-through switcher: the reason the tiles are
opaque today is that overlay text and terminal text share one pass.

## Selection: what works, what is still missing

Working, and verified with `xdotool` drags read back by `xsel` as an independent
client: drag to select (translucent wash over the cell backgrounds, under the
glyphs), double-click for the word, triple-click for the rows, middle click to
paste, `Ctrl+Shift+C` / `Ctrl+Shift+v` for the clipboard. Selecting text fills the
*primary* buffer, which is the part VTE gives the original for free.

Still open:

Completed in the clipboard pass: scrollback selection; terminal-owned selection
rotation; clearing on a tab-strip click; configurable `selection_color`; single-cell
and rectangular copies; whole-line copies including their final newline.

Still open: selection autoscroll when dragging outside the viewport, and keyboard
selection. Alt+drag already makes a rectangular selection.

## Clipboard (done)

- Native Wayland ownership on a private worker connection: prefer ext-data-control,
  fall back to wlr-data-control, then wl-clipboard when neither is advertised.
  X11 retains xsel. The backend comes from the actual window, not environment guesses.
- Copy and paste never wait on the UI thread. Transfers have a two-second deadline,
  a 16 MiB limit, bounded queues and bounded writer threads. Failed helpers are
  killed/reaped; failures appear briefly in the terminal rather than disappearing.
- Paste remembers the requesting tab's stable ID. Closing/reordering/switching tabs
  cannot redirect a delayed paste. Clipboard and primary remain independent.
- UTF-8 and trailing newlines survive transport. PTY paste normalizes CRLF/lone CR,
  uses CR for plain Enter, and protects bracketed paste from embedded ESC/ETX.
- Real Ctrl+Shift+C/V work with winit's uppercase shifted keys; Shift+Insert pastes
  primary. Default tab-strip visibility moved to Ctrl+Alt+V to avoid the paste clash.
  Legacy explicit bindings are still loaded.
- Verified in isolated Sway against independent wl-copy/wl-paste clients, and in
  Xvfb with real xdotool keyboard/mouse events and xsel. Single-character, word,
  line/newline, primary middle-click, Unicode bracketed paste, and history selection
  were exercised through the window.

Compatibility limits: X11 still needs xsel; Wayland desktops without data-control
need wl-clipboard. Native clipboard persistence after the app exits depends on a
clipboard manager. IME and terminal mouse reporting remain separate parity work.
