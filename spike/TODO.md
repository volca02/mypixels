# Spike polish TODO

Collected from visual comparison against the original (Python/GTK/VTE)
mypixels. These are look/feel gaps, not correctness bugs. Work on them as the
spike gets usable; more feedback expected.

## Tab bar / tablist

- [ ] **Too tall.** Bar height is `tab_h = tab_font_px * 1.3 + 6.0`. Bring it
      down to match the original's slimmer bar (revisit the +6 padding and the
      1.3 line-height factor).
- [ ] **Bare-bones styling.** Match the original look: per-tab padding, the
      1px gap between tabs, the add-tab `[+]` button styling, selected vs
      unselected treatment. Currently it reads as flat rectangles.
- [ ] **Different font.** The tablist font does not match the original. Verify
      the configured `tablist_font` family is actually resolved and shaped
      (fontdb family match), and that its metrics are the ones used for width.
- [ ] **Not clickable.** Add mouse handling: clicking a tab selects it,
      clicking `[+]` opens a new tab. (winit MouseInput/WindowFocused; hit-test
      against the same tab rects used for drawing.)
- [ ] **Titles don't live-update.** Tab titles should track the terminal
      window-title like the original (they do via `Tab.title`, but confirm the
      strip re-renders on title change and matches the original's update
      cadence/formatting).

## Terminal rendering

- [x] **Cursor does not blink.** Done (activity-aware: solid while the
      terminal is producing output or right after a keypress, ~533ms on/off
      when idle). Note: this was blocked by a systemic bug, not a missing
      feature -- `RedrawRequested` only drew when a hand-rolled `wake` flag was
      set, so redraw requests that didn't set it drew nothing. That flag is
      gone; see bug 9 in RESULTS.md.
- [ ] **Prompt has extra characters.** Investigate which prompt shows stray
      glyphs (suspects: the inline rename prompt, or a terminal prompt line).
      Confirm against the original once identified.

## Notes

- Font matching for the *terminal* was previously noted too; if the terminal
  glyphs still look off vs the original, check the cosmic-text family fallback
  (a missing family silently falls back to another, changing metrics).
- Keep the Xvfb + `import` capture loop for visual checks (see RESULTS.md).
