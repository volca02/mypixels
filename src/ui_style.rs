//! Shared visual language for the terminal UI. Palette, typography, icons,
//! surfaces and control treatments live here; screens own content and layout.

#[derive(Clone, Copy)]
pub(crate) struct TermStyle {
    pub(crate) bg: egui::Color32,
    pub(crate) fg: egui::Color32,
    pub(crate) muted: egui::Color32,
    pub(crate) border: egui::Color32,
    pub(crate) accent: egui::Color32,
    pub(crate) card: egui::Color32,
    pub(crate) ok: egui::Color32,
    pub(crate) err: egui::Color32,
}

impl TermStyle {
    pub(crate) fn new(ui: &egui::Ui, ok: egui::Color32, err: egui::Color32) -> Self {
        let v = ui.visuals();
        let dark = v.text_color().intensity() > v.window_fill.intensity();
        let bg =
            if dark { v.window_fill.lerp_to_gamma(egui::Color32::from_rgb(10, 18, 23), 0.65) } else { v.window_fill };
        let fg = if dark { v.text_color().lerp_to_gamma(egui::Color32::WHITE, 0.4) } else { v.text_color() };
        Self {
            bg,
            fg,
            muted: bg.lerp_to_gamma(fg, 0.65),
            border: bg.lerp_to_gamma(fg, 0.16),
            accent: v.hyperlink_color,
            card: bg.lerp_to_gamma(fg, 0.025),
            ok,
            err,
        }
    }

    pub(crate) fn apply_controls(&self, ui: &mut egui::Ui) {
        let style = ui.style_mut();
        style.text_styles.insert(egui::TextStyle::Body, egui::FontId::monospace(13.0));
        style.text_styles.insert(egui::TextStyle::Button, egui::FontId::monospace(13.0));
        style.text_styles.insert(egui::TextStyle::Small, egui::FontId::monospace(12.0));
        style.spacing.item_spacing = egui::vec2(12.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.slider_width = 110.0;
        let v = &mut style.visuals;
        v.override_text_color = Some(self.fg);
        v.weak_text_color = Some(self.muted);
        v.panel_fill = self.bg;
        v.window_fill = self.card;
        v.window_stroke = egui::Stroke::new(1.0, self.border);
        v.window_corner_radius = egui::CornerRadius::same(9);
        v.window_shadow = egui::epaint::Shadow::NONE;
        v.extreme_bg_color = self.bg;
        v.faint_bg_color = self.card;
        v.selection.bg_fill = self.bg.lerp_to_gamma(self.accent, 0.18);
        v.selection.stroke = egui::Stroke::new(1.0, self.accent);
        v.widgets.noninteractive.fg_stroke.color = self.fg;
        v.widgets.noninteractive.bg_stroke.color = self.border;
        for (widget, amount) in [
            (&mut v.widgets.inactive, 0.03),
            (&mut v.widgets.hovered, 0.10),
            (&mut v.widgets.active, 0.18),
            (&mut v.widgets.open, 0.13),
        ] {
            widget.corner_radius = egui::CornerRadius::same(5);
            widget.bg_fill = self.bg.lerp_to_gamma(self.accent, amount);
            widget.weak_bg_fill = widget.bg_fill;
            widget.bg_stroke = egui::Stroke::new(1.0, if amount > 0.03 { self.accent } else { self.border });
            widget.fg_stroke.color = if amount > 0.03 { self.accent } else { self.fg };
        }
    }

    pub(crate) fn status(&self, done: bool, exit: Option<i32>) -> egui::Color32 {
        if !done {
            self.accent
        } else if exit.is_some_and(|e| e != 0) {
            self.err
        } else {
            self.ok
        }
    }
}

/// A single-line galley with an ellipsis, so long commands never move the
/// clock column or enlarge the sidebar. The full command remains in a tooltip.
pub(crate) fn text(ui: &egui::Ui, rect: egui::Rect, text: &str, color: egui::Color32, size: f32, strong: bool) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut text = egui::RichText::new(text).font(egui::FontId::monospace(size)).color(color);
    if strong {
        text = text.strong();
    }
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        rect.width(),
        egui::FontId::monospace(size),
    );
    let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5);
    ui.painter().with_clip_rect(ui.clip_rect().intersect(rect)).galley(pos, galley, color);
}

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Terminal,
    Plus,
    Settings,
    Dismiss,
    Layers,
    Search,
    More,
    Clock,
    Check,
    Error,
    Close,
    Copy,
    Pipe,
    Rerun,
    View,
}

pub(crate) fn icon_button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    glyph: Icon,
    label: &str,
    c: &TermStyle,
) -> egui::Response {
    let response = ui.interact(rect, ui.scope_id().with(label), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    if response.hovered() || response.has_focus() {
        ui.painter().rect(rect, 16.0, c.card, egui::Stroke::new(1.0, c.border), egui::StrokeKind::Inside);
    }
    icon(ui.painter(), glyph, rect.center(), if response.hovered() || response.has_focus() { c.accent } else { c.fg });
    response.on_hover_text(label).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small vector icons avoid missing-glyph boxes in the bundled egui font.
pub(crate) fn icon(p: &egui::Painter, icon: Icon, center: egui::Pos2, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.4, color);
    let pt = |x, y| center + egui::vec2(x, y);
    let line =
        |points: &[(f32, f32)]| p.add(egui::Shape::line(points.iter().map(|&(x, y)| pt(x, y)).collect(), stroke));
    match icon {
        Icon::Plus => {
            line(&[(-6.0, 0.0), (6.0, 0.0)]);
            line(&[(0.0, -6.0), (0.0, 6.0)]);
        }
        Icon::Terminal => {
            p.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(18.0, 16.0)),
                3.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(&[(-5.0, -4.0), (-1.0, 0.0), (-5.0, 4.0)]);
            line(&[(1.0, 4.0), (5.0, 4.0)]);
        }
        Icon::Settings => {
            p.circle_stroke(center, 5.5, stroke);
            p.circle_stroke(center, 2.0, stroke);
            for i in 0..8 {
                let angle = i as f32 * std::f32::consts::TAU / 8.0;
                let d = egui::vec2(angle.cos(), angle.sin());
                p.line_segment([center + d * 5.5, center + d * 8.5], stroke);
            }
        }
        Icon::Dismiss => {
            line(&[(-5.0, -5.0), (5.0, 5.0)]);
            line(&[(-5.0, 5.0), (5.0, -5.0)]);
        }

        Icon::Layers => {
            for y in [-5.0, 0.0, 5.0] {
                line(&[(-8.0, y), (0.0, y + 4.0), (8.0, y)]);
            }
            line(&[(-8.0, -5.0), (0.0, -9.0), (8.0, -5.0)]);
        }
        Icon::Search => {
            p.circle_stroke(pt(-2.0, -2.0), 6.0, stroke);
            line(&[(2.5, 2.5), (8.0, 8.0)]);
        }
        Icon::More => {
            for x in [-6.0, 0.0, 6.0] {
                p.circle_filled(pt(x, 0.0), 1.3, color);
            }
        }
        Icon::Clock => {
            p.circle_stroke(center, 7.0, stroke);
            line(&[(0.0, -4.5), (0.0, 0.0), (3.5, 2.0)]);
        }
        Icon::Check => {
            line(&[(-4.0, 0.0), (-1.0, 3.0), (5.0, -4.0)]);
        }
        Icon::Error => {
            p.circle_stroke(center, 7.0, stroke);
            line(&[(0.0, -4.0), (0.0, 1.0)]);
            p.circle_filled(pt(0.0, 4.0), 0.8, color);
        }
        Icon::Close => {
            line(&[(-2.5, -2.5), (2.5, 2.5)]);
            line(&[(-2.5, 2.5), (2.5, -2.5)]);
        }
        Icon::Copy => {
            p.rect_stroke(
                egui::Rect::from_min_max(pt(-6.0, -3.0), pt(3.0, 7.0)),
                1.0,
                stroke,
                egui::StrokeKind::Middle,
            );
            line(&[(-2.0, -4.0), (-2.0, -7.0), (7.0, -7.0), (7.0, 3.0), (5.0, 3.0)]);
        }
        Icon::Pipe => {
            line(&[(-5.0, -7.0), (-5.0, 7.0)]);
            line(&[(-5.0, 2.0), (4.0, -1.0), (4.0, -7.0)]);
            line(&[(1.0, -4.0), (4.0, -7.0), (7.0, -4.0)]);
            p.circle_filled(pt(-5.0, 7.0), 1.8, color);
        }
        Icon::Rerun => {
            line(&[
                (5.0, -3.0),
                (2.0, -7.0),
                (-4.0, -6.0),
                (-7.0, -1.0),
                (-6.0, 4.0),
                (-1.0, 7.0),
                (4.0, 5.0),
                (7.0, 1.0),
            ]);
            line(&[(5.0, -7.0), (5.0, -3.0), (1.0, -3.0)]);
        }
        Icon::View => {
            p.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(13.0, 13.0)),
                1.0,
                stroke,
                egui::StrokeKind::Middle,
            );
            line(&[(-3.0, -2.0), (0.0, 0.0), (-3.0, 2.0)]);
            line(&[(1.0, 3.0), (4.0, 3.0)]);
        }
    }
}

pub fn theme(cfg: &crate::config::Config, bg: [f32; 3], fg: [f32; 3]) -> egui::Visuals {
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| -> egui::Color32 {
        let c = |i: usize| ((a[i] * (1.0 - t) + b[i] * t) * 255.0).round().clamp(0.0, 255.0) as u8;
        egui::Color32::from_rgb(c(0), c(1), c(2))
    };
    // bg/fg are the terminal's *effective* colours (theme, palette and
    // overrides all resolved upstream) - exactly what the screen paints
    // with. cfg.background_color alone would ignore a themed session and
    // render the UI in default-dark while the terminal wore gruvbox.
    let accent = cfg.accent_color.to_f32();
    let tablist = cfg.tablist_background.to_f32();
    let black = [0.0f32; 3];
    let mut v = egui::Visuals::dark();
    // Panels sit between the terminal background and the tab-strip colour;
    // windows (config panel, popups) nudge up from the background so they
    // read as raised without needing an opinionated grey.
    v.panel_fill = mix(bg, tablist, 0.5);
    v.window_fill = mix(bg, fg, 0.05);
    v.window_stroke = egui::Stroke::new(1.0, mix(bg, fg, 0.25));
    v.faint_bg_color = mix(bg, fg, 0.08);
    // Text inputs darker than the background: they must read as wells.
    v.extreme_bg_color = mix(bg, black, 0.35);
    v.hyperlink_color = mix(accent, fg, 0.2);
    v.selection.bg_fill = mix(bg, accent, 0.4);
    v.selection.stroke = egui::Stroke::new(1.0, mix(bg, fg, 0.9));
    {
        let mut w = v.widgets.clone();
        let base = mix(bg, fg, 0.06);
        w.inactive.bg_fill = base;
        w.inactive.weak_bg_fill = base;
        w.inactive.bg_stroke = egui::Stroke::new(1.0, mix(bg, fg, 0.2));
        w.hovered.bg_fill = mix(bg, accent, 0.18);
        w.hovered.weak_bg_fill = mix(bg, accent, 0.18);
        w.hovered.bg_stroke = egui::Stroke::new(1.0, mix(bg, accent, 0.6));
        w.active.bg_fill = mix(bg, accent, 0.3);
        w.active.weak_bg_fill = mix(bg, accent, 0.3);
        w.active.bg_stroke = egui::Stroke::new(1.0, mix(bg, accent, 0.8));
        w.open.bg_fill = base;
        w.open.weak_bg_fill = base;
        w.open.bg_stroke = egui::Stroke::new(1.0, mix(bg, accent, 0.6));
        v.widgets = w;
    }
    {
        let non = &mut v.widgets.noninteractive;
        non.fg_stroke = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(
                (fg[0].clamp(0.0, 1.0) * 255.0) as u8,
                (fg[1].clamp(0.0, 1.0) * 255.0) as u8,
                (fg[2].clamp(0.0, 1.0) * 255.0) as u8,
            ),
        );
        // 0.37 derives weak/disabled text by tinting fg_stroke towards
        // weak_bg_fill, so setting the base above themes all three at once.
    }
    v
}

/// Convert a config RGB to egui colour (clamped: user YAML can be bold).
pub fn rgb_c32(rgb: crate::config::Rgb) -> egui::Color32 {
    let c = rgb.to_f32();
    egui::Color32::from_rgb(
        (c[0].clamp(0.0, 1.0) * 255.0) as u8,
        (c[1].clamp(0.0, 1.0) * 255.0) as u8,
        (c[2].clamp(0.0, 1.0) * 255.0) as u8,
    )
}

/// Shared endings for settings and its font picker: a neutral Cancel and
/// an accent-outlined primary action, matching history's selected actions.
pub(crate) fn actions(ui: &mut egui::Ui, width: f32, primary: &str, cancel: &str) -> (bool, bool) {
    let accent = ui.visuals().hyperlink_color;
    let base = ui.visuals().panel_fill;
    let border = ui.visuals().widgets.inactive.bg_stroke;
    let half = ((width - 10.0) / 2.0).max(0.0);
    let mut result = (false, false);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        result.1 = ui
            .add_sized([half, 34.0], egui::Button::new(cancel).fill(base).stroke(border).corner_radius(5.0))
            .clicked();
        result.0 = ui
            .add_sized(
                [half, 34.0],
                egui::Button::new(egui::RichText::new(primary).color(accent))
                    .fill(base.lerp_to_gamma(accent, 0.14))
                    .stroke(egui::Stroke::new(1.0, accent))
                    .corner_radius(5.0),
            )
            .clicked();
    });
    result
}

pub(crate) fn section(ui: &mut egui::Ui, title: &str, colors: &TermStyle, contents: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    egui::Frame::new()
        .fill(colors.card)
        .stroke(egui::Stroke::new(1.0, colors.border))
        .corner_radius(9.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width((width - 30.0).max(0.0));
            ui.label(egui::RichText::new(title).font(egui::FontId::monospace(15.0)).strong().color(colors.fg));
            ui.add_space(6.0);
            contents(ui);
        });
}

pub(crate) fn form_grid(id: &str) -> egui::Grid {
    egui::Grid::new(id).num_columns(2).spacing(egui::vec2(12.0, 4.0)).min_row_height(32.0)
}

/// Compact chrome stays readable while reserving terminal rows.
pub(crate) fn tab_height(text_height: f32, scale: f32) -> f32 {
    (text_height + 8.0 * scale).max(28.0 * scale).round()
}

impl TermStyle {
    pub(crate) fn surface(&self, p: &egui::Painter, rect: egui::Rect, radius: f32, selected: bool, hovered: bool) {
        let fill = self.bg.lerp_to_gamma(
            if selected { self.accent } else { self.fg },
            if selected {
                0.11
            } else if hovered {
                0.09
            } else {
                0.055
            },
        );
        p.rect(
            rect,
            radius,
            fill,
            egui::Stroke::new(if selected { 1.5 } else { 1.0 }, if selected { self.accent } else { self.border }),
            egui::StrokeKind::Inside,
        );
    }

    pub(crate) fn number_badge(&self, p: &egui::Painter, rect: egui::Rect, number: usize, selected: bool, size: f32) {
        p.rect(
            rect,
            4.0,
            if selected { self.accent.gamma_multiply(0.8) } else { self.bg.lerp_to_gamma(self.fg, 0.14) },
            egui::Stroke::new(1.0, self.border),
            egui::StrokeKind::Inside,
        );
        p.text(rect.center(), egui::Align2::CENTER_CENTER, number.to_string(), egui::FontId::monospace(size), self.fg);
    }
}

/// Truncated, vertically centered labels using the configured tab face or UI font.
pub(crate) fn text_with_font(ui: &egui::Ui, rect: egui::Rect, label: &str, color: egui::Color32, font: egui::FontId) {
    if rect.width() <= 0.0 {
        return;
    }
    let galley = egui::WidgetText::from(egui::RichText::new(label).font(font.clone()).color(color)).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        rect.width(),
        font,
    );
    ui.painter().with_clip_rect(ui.clip_rect().intersect(rect)).galley(
        egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5),
        galley,
        color,
    );
}

/// Tab-specific state colors resolved alongside the shared UI palette.
/// Legacy defaults adopt the new chrome; explicit color overrides still work.
pub(crate) struct TabStyle {
    pub base: TermStyle,
    pub foreground: egui::Color32,
    pub selected_foreground: egui::Color32,
    pub entangle: egui::Color32,
    pub index: egui::Color32,
    pub add: egui::Color32,
    background: egui::Color32,
    selected_background: egui::Color32,
}

impl TabStyle {
    pub fn new(ui: &egui::Ui, cfg: &crate::config::Config) -> Self {
        static DEFAULTS: std::sync::LazyLock<crate::config::Config> =
            std::sync::LazyLock::new(crate::config::Config::default);
        let defaults = &*DEFAULTS;
        let resolve = |value, default, fallback| if value == default { fallback } else { rgb_c32(value) };
        let mut base = TermStyle::new(ui, rgb_c32(cfg.col[2]), rgb_c32(cfg.col[1]));
        if cfg.tablist_background != defaults.tablist_background {
            base.bg = rgb_c32(cfg.tablist_background);
            base.card = base.bg.lerp_to_gamma(base.fg, 0.025);
            base.border = base.bg.lerp_to_gamma(base.fg, 0.16);
            base.muted = base.bg.lerp_to_gamma(base.fg, 0.65);
        }
        Self {
            foreground: resolve(cfg.tab_color_fg, defaults.tab_color_fg, base.muted),
            selected_foreground: resolve(cfg.tab_color_fg_selected, defaults.tab_color_fg_selected, base.fg),
            background: resolve(cfg.tab_color_bg, defaults.tab_color_bg, base.bg.lerp_to_gamma(base.fg, 0.055)),
            selected_background: resolve(
                cfg.tab_color_bg_selected,
                defaults.tab_color_bg_selected,
                base.bg.lerp_to_gamma(base.accent, 0.11),
            ),
            entangle: resolve(cfg.tab_color_entangle, defaults.tab_color_entangle, base.accent),
            index: resolve(cfg.tab_color_idx, defaults.tab_color_idx, base.fg),
            add: resolve(cfg.add_button_color, defaults.add_button_color, base.muted),
            base,
        }
    }

    pub fn tab(&self, p: &egui::Painter, rect: egui::Rect, selected: bool, hovered: bool) {
        let background = if selected { self.selected_background } else { self.background };
        p.rect(
            rect,
            5.0,
            if hovered { background.lerp_to_gamma(self.base.fg, 0.035) } else { background },
            egui::Stroke::new(
                if selected { 1.5 } else { 1.0 },
                if selected { self.base.accent } else { self.base.border },
            ),
            egui::StrokeKind::Inside,
        );
    }
}

pub(crate) fn picker_style(ui: &egui::Ui) -> egui::Style {
    let mut style = (**ui.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.interact_size.y = 24.0;
    style.wrap_mode = Some(egui::TextWrapMode::Truncate);
    style
}

impl TermStyle {
    pub(crate) fn card_surface(&self, p: &egui::Painter, rect: egui::Rect, selected: bool, hovered: bool) {
        let fill = if selected {
            self.bg.lerp_to_gamma(self.accent, 0.11)
        } else if hovered {
            self.bg.lerp_to_gamma(self.fg, 0.09)
        } else {
            self.bg.lerp_to_gamma(self.fg, 0.055)
        };
        if selected {
            p.rect_stroke(
                rect.expand(2.0),
                12.0,
                egui::Stroke::new(1.5, self.accent.gamma_multiply(0.35)),
                egui::StrokeKind::Outside,
            );
        }
        p.rect(
            rect,
            11.0,
            fill,
            egui::Stroke::new(if selected { 2.5 } else { 1.0 }, if selected { self.accent } else { self.border }),
            egui::StrokeKind::Inside,
        );
    }
}

pub(crate) fn toggle(ui: &mut egui::Ui, label: &str, v: &mut bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(40.0, 30.0), egui::Sense::click());
    let changed = response.clicked();
    if changed {
        *v = !*v;
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *v, label));
    let accent = ui.visuals().hyperlink_color;
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(36.0, 20.0));
    let border = if *v || response.hovered() || response.has_focus() {
        egui::Stroke::new(1.0, accent)
    } else {
        ui.visuals().widgets.inactive.bg_stroke
    };
    let fill = if *v { ui.visuals().panel_fill.lerp_to_gamma(accent, 0.24) } else { ui.visuals().panel_fill };
    ui.painter().rect(track, 10.0, fill, border, egui::StrokeKind::Inside);
    ui.painter().circle_filled(
        egui::pos2(track.left() + if *v { 26.0 } else { 10.0 }, track.center().y),
        6.0,
        if *v { accent } else { ui.visuals().weak_text_color() },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
    changed
}
