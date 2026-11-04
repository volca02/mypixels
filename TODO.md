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
3. **Everyday interaction (P1).** Done: editable rename input with selected
   initial text (egui), selection autoscroll whose speed answers the pull,
   drag-to-reorder tabs, select-all, Ctrl+wheel zoom, font dialog, themes.
   Open: **IME** and keyboard-grown text selection (Shift+arrows).
4. **Tab strip usability (P1).** Open: keep the active tab reachable with many
   tabs, configured wrapping/placement, verify title-update cadence.
5. **Task-done notifications: shipped.** An unseen BEL (window unfocused,
   or a tab you are not looking at) becomes a compositor attention request
   plus a red pulse on that tab's cell. `scripts/shell_integration.{bash,
   zsh,fish}` ring the bell on failed commands - "notify me when the build
   breaks" with no daemon, working across ssh. Full OSC 133 semantics
   OSC 133: shipped (M0-M2 of docs/graphics_osc133_implementation.md).
   vte+alacritty_terminal vendored with an osc_raw observer hook; A/B/C/D
   marks captured at dispatch with absolute lines; Ctrl+Alt+Up/Down jump
   between commands; scripts emit the marks in bash/zsh/fish alongside
   the failure bell. Kitty graphics (M3-M6) and sixel/iTerm2 (M7) remain
   on the doc's plan.
6. **Search (P2).** A reusable inline text input, incremental search, next/previous
   matches, visible match state, and cancellation back to the terminal.
7. **Multi-window: shipped.** One process hosts many windows: `Ctrl+Shift+n`,
   `mypixels`/`mypixels -n` joining over D-Bus, `Alt+Shift+w` send-tab
   (direct with two windows, egui picker with three+, "New window" detaches),
   X11 drag a tab onto another window or off the border to detach.
   Open: **TabMonitor** (Alt+Shift+m floating live snapshot).
8. **Small parity gaps: closed.** Ctrl+wheel zoom, font selector,
   `bg_tint_color`, Wayland app-id + window icon, visual bell, named
   themes with XDG discovery and derived palettes.

# Feature parity gaps (audit against the original source)

Everything else is at parity: tabs/groups/entangle/rename/move/goto, strip
hide and placement, opacity, palette + pywal + themes + live reload, the full
selection model, async clipboard, scrollback, cursor blink, the Alt-held
switcher with live real-frame previews, and HiDPI.

## Command-output observability (requested, not yet built)

The OSC 133 blocks (prompt line + exit status) are the seed; these turn
them into a queryable session memory for shell users **and** for LLM
tooling:

1. **Scrollback is small and fixed.** The library default (1000) was
   silently in use; make `history` a config key. NOT 20 000: a grid line
   costs width*24+40 bytes (full-width Vec<Cell>, 24 B asserted cell),
   so 20k lines is ~59 MB per tab at 120 cols. 5 000 (~15 MB/tab) is
   the sane default; the output pool (#2) is what makes depth cheap.
2. **Command output held in a memory pool, not files** (decided
   2026-11-04: little result files around the filesystem are worse than
   RAM). Per-tab `VecDeque<CmdBlock { exit, cwd, text, truncated, ts }>`
   fed when a block closes (D mark), LRU-evicted under a GLOBAL byte
   budget (default 32 MB, config `history_bytes`), per-block cap ~8 MB
   with an explicit truncation marker, never evicting a tab's newest
   block. The pool also removes the pressure to grow scrollback huge
   (#1): a command's text outlives its grid lines by budget, not by
   scrollback depth. Streaming extraction while a command runs (per
   input drain) is needed for blocks larger than scrollback, else the
   head of the output evicts before the block closes.
2b. **Compress pool blocks with a fast codec (LATER).** Block text is
   plain output - lz4/zstd level-1 typically gives 3-6x on terminal text,
   turning the 32 MB budget into ~100-200 MB of effective memory at a
   microsecond-per-MB cost on append. Compress per-block at close (the D
   mark), keep the in-progress block uncompressed (it is appended hot),
   decompress lazily on query. `flate2` is already in the tree (png);
   `lz4_flex` or fast-mode zstd would be the first direct addition.
   Same lever could shrink scrollback itself (kitty-style line storage),
   but that means rewriting the vendored fixed-cell grid - a separate,
   much bigger project; do NOT conflate them.
3. **`mypixels last [n] [--copy]`** over the session bus: `last | grep
   error`, `last 2 --copy`. One extraction core serves this, the
   `copy_last_output` action, and MCP - transports stay thin.
4. **Images to clipboard**: when the last command displayed a Kitty
   graphic, `--copy` (and a menu/keys action) puts the **image bytes** on
   the clipboard losslessly - the upload payload is kept per image id, so
   no screenshot round trip. Depends on the graphics store (M4 of
   docs/graphics_osc133_implementation.md), not on the renderer.
5. **MCP server endpoint over a unix pipe** for LLM observability:
   windows, tabs, commands, and command outputs (files from #2 plus live
   state). JSON-RPC thread mapping onto the same query core via the
   existing main-thread channel pattern; per-display socket naming like
   the D-Bus bus; read-only first.

Known warts: a moved shell keeps its spawn-time `$MYPIXELS_WINDOW`, so
`mypixels` typed in a window that RECEIVED a moved tab opens new tabs in the
tab's old window (running environments cannot change; noted in `move_tab`).
Cross-window tab drag needs out-of-window pointer motion: works on X11, not
under Wayland's pointer confinement -- `Alt+Shift+w` is that gesture there.
Dragging a tab onto foreign applications would need the compositor
data-device protocol.

## Interaction

- [x] **Terminal mouse reporting.** Done (uncommitted). While any of the
      `Term.mode` mouse bits (`MOUSE_REPORT_CLICK`/`MOUSE_DRAG`/`MOUSE_MOTION`)
      is on and Shift is not held, press, release, per-cell motion, and wheel
      go to the PTY instead of selecting; X10 vs SGR encoding follows
      `SGR_MOUSE`, modifier bits are 4/8/16, coordinates are 1-based viewport
      cells. A reported press returns before paste, word-select, and
      tab-strip handling; a release falls back to the last reported cell when
      the pointer left the terminal mid-drag; motion is deduped per cell
      (drag mode only while a button is held, motion mode always). The
      encoding is unit-tested (`mouse_report_tests`); `tests/ui_smoke.py` has
      an e2e scenario (the fixture enables 1002+1006 on typing `m`, then
      asserts the reported SGR sequences, the wheel report, and the Shift
      bypass) -- it needs a machine with xdotool and xsel, which the
      headless build box lacks.
- [x] **Modifier chords reach the terminal only sometimes** - fixed in `keys: the
  full xterm modifier table`: named keys (arrows/Home/End/Del/pgup/F-keys) had been
  sent *without* their modifier parameter, Ctrl/Alt+Space dodged the ctrl branch via
  NamedKey::Space, Alt+Backspace lost its ESC prefix, and egui consumed Tab with no
  surface open. Ctrl+W had always encoded 0x17 correctly; if word-kill still fails
  on niri after this build, the remaining suspect is Wayland-side modifiers only.
  Test rig: Xvfb + xte + /tmp-style xlib focus helper (no WM => no keyboard focus;
  every older keyboard e2e was silently typing into the void).
- [ ] **IME.** GTK/VTE give it free; the rewrite has none.
- [x] **Selection autoscroll.** Dragging the selection edge into a band at
      either grid edge scrolls that way, faster the deeper the pointer is
      driven into the border (the pointer cannot leave the window, so depth
      is the honest measure of the pull). The first version had both signs
      inverted and dead trigger zones; see `autoscroll_tick` and its tests.
      Keyboard-grown selection remains open (Shift+arrows) -- select-all
      (`CONTROL+SHIFT+A`) exists.
- [x] **Ctrl+wheel zoom.** Whole-point font steps, matching the zoom keys.
- [x] **Font selector.** A classic family/style/size dialog with live preview
      for both terminal and tab fonts, rewriting `term_font` / `tablist_font`.

## Windows

- [x] **`new_window`** (Ctrl+Shift+n) and **`send_tab_to_window`**
      (Alt+Shift+w): two windows means "to the other one", three+ opens the
      egui picker with a "New window" detach entry. On X11 a tab can also be
      dragged out of the window -- over another window it lands in that
      window's strip, over nothing it detaches into a fresh window. An
      emptied window closes; the last window's close ends the session.
- [ ] **TabMonitor** (Alt+Shift+m): a floating window with a live 5fps
      snapshot of one tab. The offscreen preview machinery already renders a
      complete frame per tab, so this is a window + a copy of the texture.
      Note the original relies on compositor rules matching
      `app-id="mypixels"` -- the app-id gap below goes with this.

## Tab strip

- [x] **Multiline tab labels** and row navigation. What the original actually
      does: the *strip* label is `line_wrap(False)` (one truncated line -- what
      we always drew), and its `multiline_tabs` key does not exist there at
      all; the wrapping happens on the **overview cards** (`line_wrap(True)`,
      max width, centered), now reproduced: card titles take the full string
      and wrap to two centered lines, ellipsizing only at the third. Row
      navigation (`prev_tab_row`/`next_tab_row`, Alt+Up/Down while the switcher
      is open) moves by grid rows and predates this item; outside the switcher
      those keys remain linear prev/next, which is all a one-row strip can
      mean.
- [x] **Drag-to-reorder tabs.** Press arms, release decides; live swaps keep
      neighbour order stable across wobble, and a press never steals the
      scrolled view mid-reorder.

## Configuration / identity

- [x] **`bg_tint_color`** tints the terminal background when no palette
      overrides it, as in the original.
- [x] **Wayland app-id**: `with_name("mypixels", "mypixels")` sets app-id on
      Wayland and WM_CLASS on X11.
- [x] **Window icon** (generated in code, no asset) and **visual bell** (a
      brief background lift).
- [ ] `autohide_delay` is a dead key in *both* versions (the user config
      carries it); not a gap.

## Known open correctness items (already tracked below)

Titles' live-update cadence and the rename seed; tablist advance width vs
GTK; damage-driven rendering for the idle-CPU measurement.

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

- [x] **See-through switcher.** The colour-field workaround
      (`if self.overview.is_some() { break }` in the text loop) is gone:
      `render_frame` is now two phases in one pass -- base rects, terminal text
      (`prepare`, then `draw`), then the dim/tile rects, the sampled
      thumbnails, and the tile text (second `prepare`, `draw`). Drawing *between*
      the two prepares is what makes one `TextRenderer` safe: the earlier
      failure mode was preparing twice before the first draw, which swaps the
      buffers the earlier draw already referenced. The switcher text gets its own
      pool (`ov_pool`, 64 slots) because terminal runs and tile headers are
      prepared in the same frame. Thumbnails are the configured
      `tab_overview_thumb_width/height` times the window scale: bigger tiles on a
      HiDPI display, still sampling the full-resolution offscreen frame per tab,
      so they get sharper, not stretched.
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
opaque today is that overlay text and terminal text share one pass. (Now
closed -- the render is two phases, see "Still open" above.)

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
