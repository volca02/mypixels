# Color themes (work in progress)

In `~/.mypixels`, select a preset and reload with Ctrl+Shift+R:

```yaml
theme: catppuccin-mocha
```

Presets: `default`, `badwolf`, `dracula`, `catppuccin-mocha`,
`catppuccin-latte`. Existing explicit color settings override presets;
remove those settings to use all of a preset's colors.

Use `theme: /path/to/theme.yml` for a custom file, or put `name.yml` under
`~/.config/mypixels/themes/` and select `theme: name`. Copy a preset here as a
starting point. Custom files reload automatically, including atomic replacements.
Invalid reloads keep the last valid configuration.

Use `theme: wal` for `~/.cache/wal/colors.yml`, or set `wal_palette` to another
pywal YAML file. Explicit pywal palettes take precedence over config colors,
matching Python mypixels. With no `theme` key, the default pywal cache is loaded
when present. Set `wal_palette: ''` to disable it. Explicit presets skip implicit
pywal loading.

Terminal, cursor, selection, tabs, and the Alt-held visual switcher share the
theme. Application OSC overrides remain supported. File monitoring runs on a
worker thread.

Palette sources: [Dracula](https://github.com/dracula/alacritty),
[Catppuccin](https://github.com/catppuccin/alacritty). UI colors are adapted.

Validation: debug build and isolated Xvfb theme smoke test pass. Release binary
has not been rebuilt for this change; final visual polish remains unfinished.
