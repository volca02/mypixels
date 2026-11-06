# Terminal UI styling

`src/ui_style.rs` owns the reusable visual language: the theme palette,
typography, vector icons, outlined surfaces, number badges, action rows, form
sections, toggles, and picker density. Change these components here so history,
settings, the switcher, and the tab strip stay consistent. Screen modules own
content, state, and layout rather than copies of these treatments.

`TermStyle` resolves the effective terminal theme. `TabStyle` adds the strip's
state colors and preserves explicit tab color overrides; legacy default colors
adopt the shared chrome. Font previews and tab labels use the same installed
font provider, so the configured tab font remains effective.

`src/tab_bar.rs` lays out and draws the compact strip through egui. It provides
clipped geometry in logical points to the window controller. The controller
converts this to physical pixels and retains native drag/reorder/detach input;
no tab labels or backgrounds pass through the terminal's glyphon/wgpu renderer.
The strip is at least 28 logical pixels high and grows for larger configured
fonts. Horizontal scrolling keeps the active tab and close control reachable.

Sidebars and the tab strip remain passive keyboard surfaces. Explicitly focused
text and number editors own editing input; panel shortcuts stay global.
