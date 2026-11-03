# Color themes

Select a theme in the settings panel (COLORS → Preset) or write it in the
configuration file, then Ctrl+Shift+R to reload:

```yaml
theme: gruvbox-dark
```

The configuration file lives at `$XDG_CONFIG_HOME/mypixels/config.yml`
(`~/.config/mypixels/config.yml`). A legacy `~/.mypixels` from before the move
keeps working until an XDG file exists; saving always writes the file that was
loaded.

Presets: `default`, `badwolf`, `dracula`, `catppuccin-mocha`,
`catppuccin-latte`, `gruvbox-dark`, `nord`, `wal`. Explicit color settings in
the config override the theme — remove them to accept a preset wholesale.
Picking a preset in the panel applies it live and records it as `theme:` on
save, with the resolved colors written alongside so the file stays readable
without theme support.

## Where theme files live

`theme: name` resolves, in order: built-in, `~/.config/mypixels/themes/`,
`~/.local/share/mypixels/themes/`, then `$XDG_DATA_DIRS`
(`/usr/local/share`, `/usr/share`) — so packagers install into
`<prefix>/share/mypixels/themes/` and the picker lists them automatically.
`theme: /path/to/theme.yml` works too. Files reload live, including atomic
replacements; an invalid reload keeps the last valid configuration.

## The spec

```yaml
foreground_color: '#ebdbb2'
background_color: '#282828'
cursor_color: '#928374'      # optional, defaults to foreground
selection_color: '#665c54'   # optional, defaults to accent
accent_color: '#fabd2f'      # optional, defaults to palette blue
palette:
  - '#282828'
  - '#cc241d'
  # ...
```

Tab-strip and switcher colors are derived from these (blend-toward-foreground,
so light themes keep contrast); a theme may also set them directly with the
same key names the config uses (`tab_color_bg`, ...). Fonts, bindings and
geometry can't be themed — color only, on purpose.

### Short palettes are completed, not rejected

You rarely hand-pick sixteen colors; the completion is algorithmic:

* **16 or more** — taken verbatim, in ANSI order.
* **exactly 8** — the classic scheme: darks stand, brights are derived by
  lifting lightness (Lab L* +26) while keeping hue and chroma.
* **any other length** — each of the sixteen slots takes the *nearest* given
  color, distance in CIE Lab, i.e. nearest in gamut and brightness at once
  (the xterm defaults are the reference for what "slot 9" means).

## pywal

`theme: wal` loads `~/.cache/wal/colors.yml`; `wal_palette:` points at another
pywal file, `wal_palette: ''` disables the implicit cache. Pywal palettes win
over config colors, matching the Python original.

Terminal, cursor, selection, tabs and the Alt-held switcher all follow the
theme; OSC palette overrides from applications still apply on top.

Palette sources: [Dracula](https://github.com/dracula/alacritty),
[Catppuccin](https://github.com/catppuccin/alacritty),
[Gruvbox](https://github.com/morhetz/gruvbox),
[Nord](https://www.nordtheme.com/docs/colors-and-palettes).
