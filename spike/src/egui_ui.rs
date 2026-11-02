//! egui integration: egui draws the configuration window and the tab
//! switcher overlay. The terminal and tab strip stay on the app's own
//! pipeline; egui's output is composited on top in the same render pass
//! (see `Gpu::render_frame`). Events are forwarded to egui first: while an
//! egui surface is open and focused it claims its own keys and clicks,
//! everything else falls through to the app.

use egui::{Context, FullOutput, ViewportId};

/// The egui side of the app: context, winit event state and the wgpu
/// renderer that paints egui's tessellation.
pub struct Egui {
    pub ctx: Context,
    pub state: egui_winit::State,
    pub renderer: egui_wgpu::Renderer,
    /// Configuration window visibility (persists across frames).
    pub config_open: bool,
    /// One-shot flags set by the UI, consumed by `App::draw`.
    pub config_changed: bool,
    pub save_requested: bool,
    pub revert_requested: bool,
    pub ui_state: UiState,
    /// The card the pointer clicked on the previous frame (one-shot).
    pub switcher_pick: Option<usize>,
}

/// One frame of egui output, ready for `Gpu::render_frame`: tessellated
/// paint jobs plus the texture deltas that must be uploaded first.
pub struct EguiFrame<'a> {
    pub renderer: &'a mut egui_wgpu::Renderer,
    /// Mutable: `Gpu::render_frame` uploads the texture deltas and then
    /// clears the container -- epaint 0.37 panics (debug builds) if a
    /// TexturesDelta is dropped with unapplied deltas still in it.
    pub full: &'a mut FullOutput,
    pub paint_jobs: Vec<egui::epaint::ClippedPrimitive>,
    pub screen: egui_wgpu::ScreenDescriptor,
}

/// What the configuration UI asked for this frame.
pub struct ConfigEvents {
    pub apply: bool,
    pub save: bool,
    pub revert: bool,
}

/// Ephemeral UI state that must survive across frames: in-progress text
/// drafts. egui's TextEdit takes its `&mut String` as the source of truth
/// (only the cursor lives in widget state), so a draft re-created per frame
/// would lose every keystroke.
pub struct UiState {
    pub term_font_draft: String,
    pub tab_font_draft: String,
}

impl Egui {
    pub fn init(
        event_loop: &winit::event_loop::ActiveEventLoop,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        scale: f32,
    ) -> Self {
        let ctx = Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let state = egui_winit::State::new(
            ctx.clone(),
            ViewportId::ROOT,
            event_loop,
            Some(scale),
            None,
            None,
        );
        // No MSAA, no depth: egui feathers its own edges, and the frame is
        // 2D. No dithering: it assumes sRGB output and would shift tones
        // against the app's hand-drawn colours.
        let mut renderer = egui_wgpu::Renderer::new(
            device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                predictable_texture_filtering: false,
            },
        );
        // Warm the font atlas now. egui loads and rasterises its bundled
        // fonts lazily on the first pass that lays out text, which would
        // hitch the first config open by well over a frame. Lay the whole
        // window out once against a default config and upload the font
        // textures, so the first real open is ordinary UI work.
        {
            let mut warm_cfg = crate::config::Config::default();
            let mut warm_st = UiState {
                term_font_draft: String::new(),
                tab_font_draft: String::new(),
            };
            let mut open = true;
            let mut full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_max(
                        egui::pos2(0.0, 0.0),
                        egui::pos2(1200.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    config_window(ui.ctx(), &mut open, &mut warm_cfg, &mut warm_st);
                },
            );
            apply_textures(device, queue, &mut renderer, &mut full);
        }

        Self {
            ctx,
            state,
            renderer,
            config_open: false,
            config_changed: false,
            save_requested: false,
            revert_requested: false,
            ui_state: UiState {
                term_font_draft: String::new(),
                tab_font_draft: String::new(),
            },
            switcher_pick: None,
        }
    }

    /// Feed one winit event to egui. Returns true when egui claims the
    /// event (config window open and under the pointer / focused) and the
    /// app should not handle it.
    pub fn on_event(&mut self, window: &winit::window::Window, event: &winit::event::WindowEvent) -> bool {
        self.state.on_window_event(window, event).consumed
    }

    /// The card the pointer clicked on the previous frame, if any.
    pub fn take_switcher_pick(&mut self) -> Option<usize> {
        self.switcher_pick.take()
    }
}

/// One tab card in the switcher overlay.
pub struct SwitcherCard {
    pub title: String,
    /// The tab that is actually active (marked, like the strip does).
    pub active: bool,
    /// The entangled-tab marker the strip shows.
    pub marker: bool,
    /// The tab's working directory, the card's third line when present.
    pub cwd: Option<String>,
    /// A readback of the tab's live frame: the texture plus its pixel size.
    /// The size follows the terminal area's own aspect, so the image needs
    /// no letterboxing or cropping.
    pub thumb: Option<(egui::TextureHandle, (u32, u32))>,
}

// Card geometry. The card's text column is measured (see `measure_card`) and
// the thumbnail adds its own width, so a card's size is known before layout:
// that is what makes the row grouping and centring exact.
const CARD_PAD: f32 = 10.0;
/// The widest the text column may be; longer titles truncate.
const TITLE_MAX: f32 = 140.0;
const CARD_TXT_GAP: f32 = 8.0;
const CARD_GAP: f32 = 12.0;

/// The card thumbnail's size, in points. The same formula the Python
/// original's overview uses: the configured box (`tab_overview_thumb_*`),
/// capped so four fit across the panel, with the terminal's own aspect
/// fitted inside the box -- so the preview never letterboxes.
pub fn thumbnail_size(panel_w: f32, aspect: f32, max_tw: u32, max_th: u32) -> (f32, f32) {
    if aspect <= 0.0 {
        return (0.0, 0.0);
    }
    let box_w = (max_tw as f32).min(panel_w / 4.0 - 20.0).max(48.0);
    let box_h = (max_th as f32).min(box_w * 0.75).max(1.0);
    if box_w / box_h > aspect {
        (box_h * aspect, box_h)
    } else {
        (box_w, box_w / aspect)
    }
}

/// A card's exact width: pads plus the wider of the text block and the
/// thumbnail (the text sits on top, the image below it).
fn card_width(card: &SwitcherCard, text_w: f32) -> f32 {
    let tw = card.thumb.as_ref().map_or(0.0, |(_, (tw, _))| *tw as f32);
    text_w.max(tw) + CARD_PAD * 2.0
}

/// A card's exact height: pads plus the text lines, plus the thumbnail
/// below them when present.
fn card_height(card: &SwitcherCard, text_h: f32) -> f32 {
    let th = card.thumb.as_ref().map_or(0.0, |(_, (_, th))| *th as f32);
    text_h + if th > 0.0 { CARD_TXT_GAP + th } else { 0.0 } + CARD_PAD * 2.0
}

/// One line of card text, laid out truncated to `TITLE_MAX`. `measure_card`
/// and `text_column` both go through this, so the measured width and the
/// drawn width are the same number.
fn line_galley(
    ui: &egui::Ui,
    text: &str,
    font: egui::TextStyle,
    color: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let font_id = egui::FontSelection::Style(font).resolve(ui.style());
    let mut job = egui::text::LayoutJob::simple(
        text.to_string(),
        font_id,
        color,
        TITLE_MAX,
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.fonts_mut(|f| f.layout_job(job))
}

/// The card's text block: the title (the entangled `*` marker included), a
/// "tab N" caption, and the working directory when present. Returns the
/// block's (width, height); the width is the widest line, already truncated
/// to `TITLE_MAX`. (In egui 0.37 `strong` is a colour, not a font, so the
/// selected card measures the same.)
fn measure_card(ui: &egui::Ui, index: usize, card: &SwitcherCard) -> (f32, f32) {
    let text_color = ui.style().visuals.text_color();
    let weak_color = ui.style().visuals.weak_text_color();
    let mut title = if card.marker {
        format!("* {}", card.title)
    } else {
        card.title.clone()
    };
    if title.is_empty() {
        title = format!("tab {}", index + 1);
    }
    let g_title = line_galley(ui, &title, egui::TextStyle::Body, text_color);
    let g_cap = line_galley(
        ui,
        &format!("tab {}", index + 1),
        egui::TextStyle::Small,
        weak_color,
    );
    let g_cwd = card.cwd.as_ref().map(|c| {
        line_galley(
            ui,
            c,
            egui::TextStyle::Small,
            egui::Color32::from_rgb(98, 102, 120),
        )
    });
    let w = g_title.size().x
        .max(g_cap.size().x)
        .max(g_cwd.as_ref().map(|g| g.size().x).unwrap_or(0.0));
    let gap = ui.style().spacing.item_spacing.y;
    let h = g_title.size().y
        + g_cap.size().y
        + gap
        + g_cwd.as_ref().map(|g| g.size().y + gap).unwrap_or(0.0);
    (w, h)
}

/// The tab switcher overlay, drawn on the root ui. A `CentralPanel` covers
/// the whole viewport by construction (there are no other panels), so the
/// dim backdrop is exactly the terminal area -- no window to size. The
/// backdrop rect is claimed as clickable, so clicks never leak through to
/// the terminal. Clicking a card reports that tab through `pick`; keyboard
/// stays with the app (nothing here takes keyboard focus), so the Alt-held
/// state machine and its navigation keys work unchanged.
pub fn switcher_screen(
    ui: &mut egui::Ui,
    open: bool,
    selected: usize,
    dim_alpha: u8,
    accent: egui::Color32,
    cards: &[SwitcherCard],
    pick: &mut Option<usize>,
) {
    if !open || cards.is_empty() {
        return;
    }
    egui::CentralPanel::no_frame()
        .frame(egui::Frame::NONE.fill(egui::Color32::from_black_alpha(dim_alpha)))
        .show(ui, |ui| {
            // Claim the whole backdrop (see docs) *without allocating it*:
            // allocating the full rect would advance the layout cursor past
            // the entire screen and everything after it would be placed
            // off-screen. `interact` only registers a response for the
            // region, so the cards lay out as if it were not there.
            ui.interact(
                ui.max_rect(),
                egui::Id::unique("switcher_backdrop"),
                egui::Sense::click(),
            );

            // The card sizes are measured first (deterministic: `card_button`
            // lays out the same text). Which cards sit on which line is then
            // just a partition of those widths; every actual position is
            // decided by egui's layout below.
            // The card sizes are measured first (deterministic: `card_button`
            // lays out the same text). Everything below -- row grouping, row
            // widths, the block height -- is computed from those numbers, so
            // the layout is exact; egui only decides where the exact rows sit.
            let m: Vec<(f32, f32)> = cards
                .iter()
                .enumerate()
                .map(|(i, c)| measure_card(ui, i, c))
                .collect();
            let sizes: Vec<egui::Vec2> = cards
                .iter()
                .zip(m.iter())
                .map(|(c, t)| {
                    egui::vec2(card_width(c, t.0), card_height(c, t.1))
                })
                .collect();
            // The effective gap between cards: `add_space(CARD_GAP)` plus the
            // item spacing the horizontal layout adds on each side.
            let gap = CARD_GAP + 2.0 * ui.style().spacing.item_spacing.x;
            let row_gap = CARD_GAP + 2.0 * ui.style().spacing.item_spacing.y;
            let avail_w = ui.available_width();
            let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
            for (i, w) in sizes.iter().enumerate() {
                let row = rows.last().unwrap();
                let cur: f32 = row.iter().map(|&j| sizes[j].x).sum::<f32>()
                    + (row.len().saturating_sub(1)) as f32 * gap
                    + w.x;
                if cur > avail_w && !row.is_empty() {
                    rows.push(Vec::new());
                }
                rows.last_mut().unwrap().push(i);
            }
            let row_h: Vec<f32> = rows
                .iter()
                .map(|row| row.iter().map(|&i| sizes[i].y).fold(0.0, f32::max))
                .collect();
            let block_h: f32 =
                row_h.iter().sum::<f32>() + (rows.len().saturating_sub(1)) as f32 * row_gap;

            // The block, centred in the panel: each row is placed with
            // `ui.put` at its exact rect (a table row is its cells' sum),
            // horizontally centred, stacked from the block's top. No layout
            // guesswork is left to egui -- the measured sizes decide it all.
            let panel = ui.max_rect();
            let mut y = panel.min.y + ((panel.height() - block_h) / 2.0).max(8.0);
            for row in &rows {
                let row_w: f32 = row.iter().map(|&i| sizes[i].x).sum::<f32>()
                    + (row.len().saturating_sub(1)) as f32 * gap;
                let row_h: f32 = row.iter().map(|&i| sizes[i].y).fold(0.0, f32::max);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(panel.center().x - row_w / 2.0, y),
                    egui::vec2(row_w, row_h),
                );
                ui.put(rect, SwitcherRow {
                    cards,
                    indices: row,
                    sizes: &sizes,
                    selected,
                    accent,
                    pick,
                });
                y += row_h + row_gap;
            }
        });
}

/// One tab card: a rounded frame with the live thumbnail (when read back)
/// and the title, "tab N" and the working directory. The selected card
/// gets the accent stroke and the brightest fill.
/// One card. Its outer size is fixed (measured in `switcher_screen`), so the
/// allocation is exact and the content -- which may want to expand -- is
/// drawn into the card's own rect and can never push the row around.
fn card_button(
    ui: &mut egui::Ui,
    index: usize,
    card: &SwitcherCard,
    size: egui::Vec2,
    selected: bool,
    accent: egui::Color32,
    pick: &mut Option<usize>,
) {
    let fill = if selected {
        egui::Color32::from_rgb(42, 44, 54)
    } else if card.active {
        egui::Color32::from_rgb(31, 33, 41)
    } else {
        egui::Color32::from_rgb(23, 24, 30)
    };
    let stroke = if selected { accent } else { egui::Color32::from_rgb(68, 70, 84) };
    let (rect, r) = ui.allocate_exact_size(size, egui::Sense::hover());

    // Background and border. Drawn, not a `Frame`: a `Frame` sizes itself
    // from its content, and content in a horizontal flow expands to fill the
    // row, which is exactly what must not happen here.
    ui.painter().add(egui::Shape::Rect(egui::epaint::RectShape::new(
        rect,
        egui::CornerRadius::same(10),
        fill,
        egui::Stroke::new(if selected { 2.0 } else { 1.5 }, stroke),
        egui::epaint::StrokeKind::Inside,
    )));

    // The content, confined to the card's inner rect.
    ui.place(rect.shrink2(egui::vec2(CARD_PAD, CARD_PAD)), CardFace {
        index,
        card,
        selected,
    });

    if r.clicked() {
        *pick = Some(index);
    }
}

/// One row of cards. `switcher_screen` places it with `ui.put` at its exact
/// rect (a table row is its cells' sum); this lays the cards out inside it.
struct SwitcherRow<'a> {
    cards: &'a [SwitcherCard],
    indices: &'a [usize],
    sizes: &'a [egui::Vec2],
    selected: usize,
    accent: egui::Color32,
    pick: &'a mut Option<usize>,
}

impl egui::Widget for SwitcherRow<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        // The row's rect is the widget's whole rect (`ui.put` placed it).
        let rect = ui.max_rect();

        // A child ui over exactly the row's rect, laying the cards out left
        // to right (vertically centred). It is bounded by the row, so the
        // card content cannot push the row around.
        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        for (c, &i) in self.indices.iter().enumerate() {
            if c > 0 {
                row_ui.add_space(CARD_GAP);
            }
            card_button(
                &mut row_ui,
                i,
                &self.cards[i],
                self.sizes[i],
                i == self.selected,
                self.accent,
                self.pick,
            );
        }
        ui.interact(
            rect,
            egui::Id::unique("switcher_row"),
            egui::Sense::hover(),
        )
    }
}

/// The card's content: the text block on top, the thumbnail below it (the
/// image's native texture size is exactly the thumbnail size).
struct CardFace<'a> {
    index: usize,
    card: &'a SwitcherCard,
    selected: bool,
}

impl egui::Widget for CardFace<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        ui.vertical(|ui| {
            text_column(ui, self.index, self.card, self.selected);
            if let Some((handle, (tw, th))) = &self.card.thumb {
                ui.add_space(CARD_TXT_GAP);
                // The texture is stored at pixel density (points times the
                // display scale) and drawn at its point size, so it stays
                // sharp on scaled displays.
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::from(handle))
                        .fit_to_exact_size(egui::vec2(*tw as f32, *th as f32)),
                );
            }
        });
        ui.interact(
            ui.max_rect(),
            egui::Id::unique("switcher_card_face"),
            egui::Sense::hover(),
        )
    }
}

/// The card's text, drawn as pre-laid-out galleys (the same `line_galley`
/// call `measure_card` used, so the drawn width is the measured width).
fn text_column(
    ui: &mut egui::Ui,
    index: usize,
    card: &SwitcherCard,
    selected: bool,
) {
    let title_color = if selected {
        ui.style().visuals.strong_text_color()
    } else {
        ui.style().visuals.text_color()
    };
    let cap_color = ui.style().visuals.weak_text_color();
    ui.vertical(|ui| {
        let mut title = if card.marker {
            format!("* {}", card.title)
        } else {
            card.title.clone()
        };
        if title.is_empty() {
            title = format!("tab {}", index + 1);
        }
        ui.add(egui::Label::new(line_galley(
            ui,
            &title,
            egui::TextStyle::Body,
            title_color,
        )));
        ui.add(egui::Label::new(line_galley(
            ui,
            &format!("tab {}", index + 1),
            egui::TextStyle::Small,
            cap_color,
        )));
        if let Some(cwd) = &card.cwd {
            ui.add(egui::Label::new(line_galley(
                ui,
                cwd,
                egui::TextStyle::Small,
                egui::Color32::from_rgb(98, 102, 120),
            )));
        }
    });
}

/// Upload one pass's texture deltas, free the released textures, and clear
/// the container. epaint panics (debug builds) when a TexturesDelta is
/// dropped with unapplied deltas, so every FullOutput must go through here
/// -- `Gpu::render_frame` for live frames, `Egui::init` for the warm-up.
pub fn apply_textures(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut egui_wgpu::Renderer,
    full: &mut FullOutput,
) {
    for (id, deltas) in &full.textures_delta.set {
        for d in deltas {
            renderer.update_texture(device, queue, *id, d);
        }
    }
    for id in &full.textures_delta.free {
        renderer.free_texture(id);
    }
    full.textures_delta.clear();
}

/// The configuration window. Edits `cfg` live; `ConfigEvents` tells the
/// caller whether to re-apply, save to disk, or revert to the file.
pub fn config_window(
    ctx: &Context,
    open: &mut bool,
    cfg: &mut crate::config::Config,
    st: &mut UiState,
) -> ConfigEvents {
    let mut ev = ConfigEvents { apply: false, save: false, revert: false };
    if !*open {
        // Closed: build nothing. Drafts were cleared on the close frame and
        // the first open re-mirrors them from the config.
        return ev;
    }

    egui::Window::new("Configuration")
        .open(open)
        .resizable(true)
        .default_size([400.0, 640.0])
        .scroll([true, false])
        .show(ctx, |ui| {
            // Sliders have no per-widget width in 0.37; size them through
            // the spacing style (this Ui and its children only).
            ui.style_mut().spacing.slider_width = 140.0;

            // Each section is a grid whose label column is sized by the
            // widest label (see `row`), so the controls line up in a single
            // column across all sections.

            ui.heading("Terminal");
            egui::Grid::new("terminal").show(ui, |ui| {
                row(ui, "Font", |ui| {
                    font_field(ui, &mut cfg.term_font, &mut st.term_font_draft, &mut ev);
                });
                row(ui, "See-through", |ui| {
                    if ui.add(egui::Slider::new(&mut cfg.term_opacity, 0.0..=1.0)).changed() {
                        ev.apply = true;
                    }
                });
                row(ui, "Scrollback lines", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.scrollback_lines).range(0..=1_000_000)).changed() {
                        ev.apply = true;
                    }
                });

            });
            ui.heading("Colors");
            egui::Grid::new("colors").show(ui, |ui| {
                color_row(ui, "Background", &mut cfg.background_color, &mut ev);
                color_row(ui, "Foreground", &mut cfg.foreground_color, &mut ev);
                color_row(ui, "Selection", &mut cfg.selection_color, &mut ev);
                color_row(ui, "Cursor", &mut cfg.cursor_color, &mut ev);
                color_row(ui, "Accent", &mut cfg.accent_color, &mut ev);
                color_row(ui, "Background tint", &mut cfg.bg_tint_color, &mut ev);
                row(ui, "Palette", |ui| {
                    // Each button is its own grid cell.
                    for c in cfg.col.iter_mut() {
                        if color_cell(ui, c) {
                            ev.apply = true;
                        }
                    }
                });

            });
            ui.heading("Tabs");
            egui::Grid::new("tabs").show(ui, |ui| {
                row(ui, "Font", |ui| {
                    font_field(ui, &mut cfg.tablist_font, &mut st.tab_font_draft, &mut ev);
                });
                color_row(ui, "Strip", &mut cfg.tablist_background, &mut ev);
                color_row(ui, "Tab", &mut cfg.tab_color_bg, &mut ev);
                color_row(ui, "Tab (active)", &mut cfg.tab_color_bg_selected, &mut ev);
                color_row(ui, "Tab text", &mut cfg.tab_color_fg, &mut ev);
                color_row(ui, "Tab text (active)", &mut cfg.tab_color_fg_selected, &mut ev);
                color_row(ui, "Add button", &mut cfg.add_button_color, &mut ev);
                row(ui, "Title length", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.tab_title_max_length)).changed() {
                        ev.apply = true;
                    }
                });
                row(ui, "Title length (active)", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.tab_title_short_current)).changed() {
                        ev.apply = true;
                    }
                });
                checkbox_row(ui, "Strip on top", &mut cfg.tablist_top, &mut ev);
                checkbox_row(ui, "Hide strip", &mut cfg.tablist_hide, &mut ev);
                checkbox_row(ui, "Multiline tabs", &mut cfg.multiline_tabs, &mut ev);
                checkbox_row(ui, "Hide single tab", &mut cfg.hide_single_tab, &mut ev);
                checkbox_row(ui, "Focus new tabs", &mut cfg.focus_new_tabs, &mut ev);
                checkbox_row(ui, "Shorten from start", &mut cfg.tab_title_shorten_start, &mut ev);

            });
            ui.heading("Tab switcher");
            egui::Grid::new("tab_switcher").show(ui, |ui| {
                checkbox_row(ui, "Open on Alt hold", &mut cfg.tab_overview_on_alt, &mut ev);
                row(ui, "Backdrop dim", |ui| {
                    if ui.add(egui::Slider::new(&mut cfg.tab_overview_opacity, 0.0..=1.0)).changed() {
                        ev.apply = true;
                    }
                });
                row(ui, "Delay (ms)", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.tab_overview_delay_ms)).changed() {
                        ev.apply = true;
                    }
                });
                row(ui, "Thumb width", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.tab_overview_thumb_width)).changed() {
                        ev.apply = true;
                    }
                });
                row(ui, "Thumb height", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.tab_overview_thumb_height)).changed() {
                        ev.apply = true;
                    }
                });

            });
            ui.heading("Window");
            egui::Grid::new("window").show(ui, |ui| {
                row(ui, "Default width", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.default_width)).changed() {
                        ev.apply = true;
                    }
                });
                row(ui, "Default height", |ui| {
                    if ui.add(egui::DragValue::new(&mut cfg.default_height)).changed() {
                        ev.apply = true;
                    }
                });
                checkbox_row(ui, "Start maximized", &mut cfg.default_maximize, &mut ev);

            
            });ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Revert").clicked() {
                    ev.revert = true;
                }
                if ui.button("Save").clicked() {
                    ev.save = true;
                }
            });
        // The window's own close button (title-bar X) flips *open; no extra
        // Close button here -- it would also re-borrow `open` under .open().
        });
    if !*open {
        // Closed by the title-bar X this frame: discard uncommitted drafts;
        // the next open re-mirrors the config.
        st.term_font_draft.clear();
        st.tab_font_draft.clear();
    }
    ev
}

/// One aligned row inside a section [`egui::Grid`]: the label cell, the
/// control cell, end of row. The grid sizes the label column to the widest
/// label, so every control starts at the same x.
fn row(ui: &mut egui::Ui, label: &str, add_control: impl FnOnce(&mut egui::Ui)) {
    ui.label(label);
    add_control(ui);
    ui.end_row();
}

fn font_field(ui: &mut egui::Ui, cfg_value: &mut String, draft: &mut String, ev: &mut ConfigEvents) {
    // The draft persists across frames (see UiState); egui writes the
    // user's keystrokes into it. Commit to `cfg` only on Enter or loss of
    // focus -- a font change rebuilds the whole font system and glyph
    // pools, far too heavy per keystroke. While idle the field mirrors the
    // live config value.
    let r = ui.add(egui::TextEdit::singleline(draft).desired_width(200.0));
    // 0.37 has no Response::succeeded: Enter with focus is the commit.
    let enter = r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if r.lost_focus() || enter {
        if *draft != *cfg_value {
            cfg_value.clone_from(draft);
            ev.apply = true;
        }
        *draft = cfg_value.clone();
    } else if !r.has_focus() {
        *draft = cfg_value.clone();
    }
}

fn color_row(ui: &mut egui::Ui, label: &str, c: &mut crate::config::Rgb, ev: &mut ConfigEvents) {
    row(ui, label, |ui| {
        if color_cell(ui, c) {
            ev.apply = true;
        }
    });
}

/// The color button alone (the control side of an aligned row).
fn color_cell(ui: &mut egui::Ui, c: &mut crate::config::Rgb) -> bool {
    let mut rgb: [u8; 3] = [c.0, c.1, c.2];
    let changed = ui.color_edit_button_srgb(&mut rgb).changed();
    if changed {
        *c = crate::config::Rgb(rgb[0], rgb[1], rgb[2]);
    }
    changed
}

fn checkbox_row(ui: &mut egui::Ui, label: &str, v: &mut bool, ev: &mut ConfigEvents) {
    row(ui, label, |ui| {
        // The label sits in the label column; the box carries no text.
        if ui.checkbox(v, "").changed() {
            ev.apply = true;
        }
    });
}




#[cfg(test)]
mod regression {
    use super::*;

    /// One layout pass of the config window at a given screen size.
    fn pass(ctx: &Context, w: f32, h: f32, open: bool) {
        let mut st = UiState { term_font_draft: String::new(), tab_font_draft: String::new() };
        let mut cfg = crate::config::Config::default();
        let mut open_local = open;
        let mut full = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(w, h))),
                ..Default::default()
            },
            |ui| { config_window(ui.ctx(), &mut open_local, &mut cfg, &mut st); },
        );
        full.textures_delta.clear();
    }

    // Regression: a row that requested an infinite size blew the layout up
    // to NaN (INF - INF) and tripped egui's `frame_rect is NaN` assert. The
    // sequence below mirrors the real one: init warm-up, live frames at the
    // window's real size, close, reopen.
    #[test]
    fn config_window_layout_sequence() {
        let ctx = Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        pass(&ctx, 1200.0, 800.0, true); // warm-up
        pass(&ctx, 1280.0, 720.0, true); // first live open
        pass(&ctx, 1280.0, 720.0, true);
        pass(&ctx, 900.0, 600.0, true); // resize
        pass(&ctx, 900.0, 600.0, false); // close
        pass(&ctx, 900.0, 600.0, true); // reopen
    }

    // The switcher is a full-viewport CentralPanel with a row of cards whose
    // widths vary (thumbnail present or not); same class of size request to
    // keep NaN-free. Some cards carry a real texture so the image path runs.
    #[test]
    fn switcher_screen_layout() {
        let ctx = Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        // The same config values and aspect main.rs uses; a wide and a
        // narrow screen so both the row grouping and the per-row centring
        // are exercised.
        let aspect = 1.78;
        for (w, h) in [(1920.0, 1080.0), (560.0, 480.0)] {
            let (ptw, pth) = thumbnail_size(w, aspect, 300, 225);
            let (tw, th) = (ptw.round() as u32, pth.round() as u32);
            let tex = ctx.load_texture(
                format!("test-thumb-{tw}x{th}"),
                egui::ColorImage::from_rgba_unmultiplied(
                    [tw as usize, th as usize],
                    &[7u8, 9, 11, 255].repeat((tw * th) as usize),
                ),
                egui::TextureOptions::LINEAR,
            );
            let cards: Vec<SwitcherCard> = (0..8)
                .map(|i| SwitcherCard {
                    title: format!("a much longer tab title number {i}"),
                    active: i == 1,
                    marker: i == 3,
                    cwd: if i % 2 == 0 { Some(format!("~/projects/x{i}")) } else { None },
                    thumb: if i < 3 { Some((tex.clone(), (tw, th))) } else { None },
                })
                .collect();
            for _ in 0..3 {
                let mut pick = None;
                let mut full = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_max(
                            egui::pos2(0.0, 0.0),
                            egui::pos2(w, h),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        switcher_screen(
                            ui,
                            true,
                            1,
                            76,
                            egui::Color32::WHITE,
                            &cards,
                            &mut pick,
                        );
                    },
                );
                // The card frames must actually be ON the screen. (The dim
                // backdrop is a full-screen rect and the text is a galley,
                // so rect shapes of card size are the cards themselves; this
                // is what catches a layout that slides the row off-screen.)
                let on_screen = full
                    .shapes
                    .iter()
                    .filter(|cs| {
                        matches!(
                            cs,
                            egui::epaint::ClippedShape {
                                shape: egui::Shape::Rect(r),
                                ..
                            } if (80.0..400.0).contains(&r.rect.size().x)
                                && (40.0..300.0).contains(&r.rect.size().y)
                                && r.rect.min.y >= 0.0
                                && r.rect.max.y <= h + 1.0
                                && r.rect.min.x >= 0.0
                                && r.rect.max.x <= w + 1.0
                        )
                    })
                    .count();
                assert!(
                    on_screen >= cards.len(),
                    "only {on_screen} card frames on screen of {}",
                    cards.len()
                );
                // Thumbnails sit BELOW their card's text (the card layout is
                // text on top, image at the bottom), not beside it: each
                // (64,52) image rect is inside its card, starting at the
                // left and well below the card's top edge.
                let rects: Vec<egui::Rect> = full
                    .shapes
                    .iter()
                    .filter_map(|cs| match cs {
                        egui::epaint::ClippedShape { shape: egui::Shape::Rect(r), .. } => {
                            Some(r.rect)
                        }
                        _ => None,
                    })
                    .collect();
                let cards_r: Vec<&egui::Rect> = rects
                    .iter()
                    .filter(|r| {
                        (80.0..400.0).contains(&r.size().x)
                            && (40.0..300.0).contains(&r.size().y)
                    })
                    .collect();
                let images: Vec<&egui::Rect> = rects
                    .iter()
                    .filter(|r| {
                        (tw as f32 - 1.0..tw as f32 + 1.0).contains(&r.size().x)
                            && (th as f32 - 1.0..th as f32 + 1.0).contains(&r.size().y)
                    })
                    .collect();
                let with_thumbs = cards.iter().filter(|c| c.thumb.is_some()).count();
                assert_eq!(images.len(), with_thumbs, "thumbnail images missing");
                for img in &images {
                    let card = cards_r
                        .iter()
                        .find(|c| c.contains(img.center()))
                        .expect("thumbnail outside its card");
                    assert!(
                        img.min.x >= card.min.x - 1.0 && img.min.x <= card.min.x + 30.0,
                        "thumbnail not left-aligned in card: img {:?} card {:?}",
                        img,
                        card
                    );
                    assert!(
                        img.min.y > card.min.y + 24.0,
                        "thumbnail not below the text: img {:?} card {:?}",
                        img,
                        card
                    );
                }
                full.textures_delta.clear();
            }
        }
    }
}

