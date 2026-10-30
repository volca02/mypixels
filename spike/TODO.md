# Spike polish TODO

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

- [ ] **Titles don't live-update.** They track the terminal title via `Tab.title`,
      but confirm the strip re-renders on change at the original's cadence, and that
      the rename prompt seeds correctly (renaming a tab titled `shell` produced
      `shellworktree` -- the draft appears to append rather than replace).
- [ ] **Tablist font is not the original's metrics.** The family resolves, but the
      strip's advance width still reads slightly different from GTK's Terminus.
- [ ] Terminal cell height is 14px where VTE measures 15. skrifa exposes hhea
      metrics; Pango uses OS/2 win metrics. One row per ~15 rows of density.
- [ ] Search panel (deprioritised).
- [ ] Scrollback view, selection, IME, copy/paste -- the spike is still not a full
      terminal.

## Notes

- Keep the Xvfb + `import` capture loop for visual checks (see RESULTS.md). Capture
  by window id (`xdotool search --name`), not root: the reference app and the spike
  overlap on the same display.
- The reference app runs offscreen with `HOME=/tmp/pyhome`, whose `.zshrc` prints a
  ruler; that is how the 7x15 cell and 21px strip were measured.
