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
    /// The installed fonts the font popup can preview, from the app's own
    /// font database (see `fonts::font_choices`).
    pub font_choices: Vec<crate::fonts::FontChoice>,
    /// Last frame's (panel open, popup open), for close-edge focus release.
    pub open_prev: (bool, bool),
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

/// Ephemeral UI state that must survive across frames.
pub struct UiState {
    /// The open font popup, if any (at most one at a time).
    pub font_popup: Option<FontPopup>,
}

/// The font selector popup, opened from one of the two font rows. It works
/// like the classic font-selection dialog: family, style, size, and a live
/// preview; nothing reaches the config until OK.
pub struct FontPopup {
    /// The row it belongs to; the chosen description is written there.
    pub which: FontWhich,
    /// egui's open flag; click-outside (or OK/Cancel) flips it to false.
    pub open: bool,
    /// The filter box contents, persisted per frame while open.
    pub search: String,
    // ---- Draft (the popup's own selection, not yet the config's) ----
    /// The drafted family name.
    pub family: String,
    /// Index of `family` in the choices slice ([`usize::MAX`] when the
    /// drafted family is not installed -- the config can name anything).
    /// Carrying the index keeps the per-frame path free of name lookups.
    pub family_idx: usize,
    /// Index into the family's face list (style).
    pub style: usize,
    /// Drafted size in points.
    pub size_pt: f32,
    /// The rendered family list: `(filter, drafted family, choice indices)`.
    /// Rebuilt only when one of its two inputs changes -- a keystroke or a
    /// family click -- never per frame, over what can be hundreds of
    /// families. (Filtering an already-sorted list stays sorted; the
    /// drafted family is rotated to the front.)
    pub list_cache: Option<(String, String, Vec<usize>)>,
}

/// Which font row a [`FontPopup`] belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FontWhich {
    Term,
    Tablist,
}

/// egui family name for one face's preview. Faces live under a name built
/// from the family, weight and slant so a family's styles cannot overwrite
/// each other in egui's face store (which dedupes installs by name).
pub fn preview_family_name(family: &str, weight: u16, slant: crate::fonts::DbStyle) -> String {
    let slope = match slant {
        crate::fonts::DbStyle::Normal => 'n',
        crate::fonts::DbStyle::Italic => 'i',
        crate::fonts::DbStyle::Oblique => 'o',
    };
    format!("{family}\u{1}{weight}{slope}")
}

/// Serves the installed font files to egui's text layout, on first use.
///
/// The font popup previews families and styles egui does not ship; this
/// provider is where their bytes come from. `fonts_for_family` is asked once
/// per family name at layout time, so each previewed file is read exactly
/// once per session and only when it is actually shown.
struct FontPreviewProvider {
    fonts: std::sync::Mutex<std::collections::HashMap<String, (std::path::PathBuf, u32)>>,
}

impl egui::FontProvider for FontPreviewProvider {
    fn fonts_for_family(&self, family: &egui::FontFamily) -> Vec<egui::FontInsert> {
        // Match ergonomics: `name` is a `&Arc<str>`.
        let egui::FontFamily::Name(name) = family else {
            return Vec::new();
        };
        let Some((path, face)) = self.fonts.lock().unwrap().remove(name.as_ref()) else {
            return Vec::new();
        };
        match std::fs::read(&path) {
            Ok(bytes) => vec![egui::FontInsert {
                name: name.to_string(),
                data: egui::FontData::from_blob(std::sync::Arc::new(bytes), face),
                families: vec![egui::InsertFontFamily {
                    family: egui::FontFamily::Name(std::sync::Arc::from(&**name)),
                    priority: egui::FontPriority::Highest,
                }],
            }],
            Err(err) => {
                log::warn!("font preview: cannot read {path:?}: {err}");
                Vec::new()
            }
        }
    }
}

impl Egui {
    pub fn init(
        event_loop: &winit::event_loop::ActiveEventLoop,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        scale: f32,
        font_choices: Vec<crate::fonts::FontChoice>,
    ) -> Self {
        let ctx = Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        // The font popup previews any installed family/style; this provider
        // hands the bytes over to egui's layout when a preview is first
        // drawn. One entry per face, under its preview family name.
        ctx.add_font_provider(std::sync::Arc::new(FontPreviewProvider {
            fonts: std::sync::Mutex::new(
                font_choices
                    .iter()
                    .flat_map(|c| {
                        c.faces.iter().map(move |f| {
                            (
                                preview_family_name(&c.name, f.weight, f.slant),
                                (f.path.clone(), f.index),
                            )
                        })
                    })
                    .collect(),
            ),
        }));
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
        // hitch the first config open by well over a frame. Lay the whole panel
        // window out once against a default config and upload the font
        // textures, so the first real open is ordinary UI work.
        {
            let mut warm_cfg = crate::config::Config::default();
            let mut warm_st = UiState { font_popup: None };
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
                    config_window(ui, &mut open, &mut warm_cfg, &mut warm_st, &[]);
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
            ui_state: UiState { font_popup: None },
            switcher_pick: None,
            font_choices,
            open_prev: (false, false),
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
/// Release egui keyboard focus. A TextEdit that vanishes with its panel or
/// popup does NOT release focus by itself, and while egui holds focus,
/// egui-winit reports every key event as consumed -- the app then drops all
/// keystrokes and the terminal silently stops receiving input ("letters
/// vanish"). Call this whenever a UI region that could own the keyboard
/// steps aside, or when the app claims a click for itself.
pub fn release_focus(ctx: &egui::Context) {
    if let Some(id) = ctx.memory(|m| m.focused()) {
        ctx.memory_mut(|m| m.surrender_focus(id));
        eprintln!("DBG release: was focused {:?}, now focused {:?}", id, ctx.memory(|m| m.focused()));
    } else {
        eprintln!("DBG release: no focused id");
    }
}

/// Edge-triggered release: fires when the config panel or the font popup
/// transitions open -> closed. `prev` is the previous frame's state, owned
/// by the caller (the app).
pub fn release_focus_if_closed(
    ctx: &egui::Context,
    open: bool,
    popup_open: bool,
    prev: &mut (bool, bool),
) {
    if (prev.0 && !open) || (prev.1 && !popup_open) {
        release_focus(ctx);
    }
    *prev = (open, popup_open);
}

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

/// The configuration panel: docked to the right edge, full window height,
/// resized by dragging its edge — nothing to drag around. Edits `cfg` live;
/// `ConfigEvents` tells the caller whether to re-apply, save to disk, or
/// revert to the file.
pub fn config_window(
    ui: &mut egui::Ui,
    open: &mut bool,
    cfg: &mut crate::config::Config,
    st: &mut UiState,
    choices: &[crate::fonts::FontChoice],
) -> ConfigEvents {
    let mut ev = ConfigEvents { apply: false, save: false, revert: false };
    if !*open {
        // Closed: build nothing. Any open popup was dropped on the close
        // frame, so a reopen starts clean.
        return ev;
    }

    egui::Panel::right("config_panel")
        .resizable(true)
        .default_size(380.0)
        .min_size(300.0)
        .max_size(640.0)
        .show(ui, |ui| {
            // Sliders have no per-widget width in 0.37; size them through
            // the spacing style (this Ui and its children only).
            ui.style_mut().spacing.slider_width = 140.0;

            // The header is pinned (outside the scroll area) so the close
            // button stays reachable however far down the user has scrolled.
            ui.horizontal(|ui| {
                ui.heading("Configuration");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("×").clicked() {
                        *open = false;
                    }
                });
            });
            ui.separator();

            // Each section is a grid whose label column is sized by the
            // widest label (see `row`), so the controls line up in a single
            // column across all sections.
            egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Terminal");
            egui::Grid::new("terminal").show(ui, |ui| {
                font_row(
                    ui,
                    "Font",
                    &mut cfg.term_font,
                    FontWhich::Term,
                    st,
                    choices,
                    &mut ev,
                );
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
            // Every color chooser lives in this one section, grouped by
            // where it paints; the other sections stay colour-free.
            ui.heading("Colors");
            ui.strong("Terminal");
            egui::Grid::new("colors-terminal").show(ui, |ui| {
                color_row(ui, "Background", &mut cfg.background_color, &mut ev);
                color_row(ui, "Foreground", &mut cfg.foreground_color, &mut ev);
                color_row(ui, "Selection", &mut cfg.selection_color, &mut ev);
                color_row(ui, "Cursor", &mut cfg.cursor_color, &mut ev);
                color_row(ui, "Accent", &mut cfg.accent_color, &mut ev);
                color_row(ui, "Background tint", &mut cfg.bg_tint_color, &mut ev);
                row(ui, "Palette", |ui| {
                    // Two rows of eight: one row of sixteen is wider than
                    // the narrowest panel.
                    for chunk in cfg.col.chunks_mut(8) {
                        ui.horizontal(|ui| {
                            for c in chunk.iter_mut() {
                                if color_cell(ui, c) {
                                    ev.apply = true;
                                }
                            }
                        });
                    }
                });
            });
            ui.strong("Tab strip");
            egui::Grid::new("colors-tabs").show(ui, |ui| {
                color_row(ui, "Strip", &mut cfg.tablist_background, &mut ev);
                color_row(ui, "Tab", &mut cfg.tab_color_bg, &mut ev);
                color_row(ui, "Tab (active)", &mut cfg.tab_color_bg_selected, &mut ev);
                color_row(ui, "Tab text", &mut cfg.tab_color_fg, &mut ev);
                color_row(ui, "Tab text (active)", &mut cfg.tab_color_fg_selected, &mut ev);
                color_row(ui, "Add button", &mut cfg.add_button_color, &mut ev);
            });
            ui.heading("Tabs");
            egui::Grid::new("tabs").show(ui, |ui| {
                font_row(
                    ui,
                    "Font",
                    &mut cfg.tablist_font,
                    FontWhich::Tablist,
                    st,
                    choices,
                    &mut ev,
                );
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

            });
            }); // ScrollArea contents

            // Pinned at the panel bottom, outside the scroll area.
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Revert").clicked() {
                    ev.revert = true;
                }
                if ui.button("Save").clicked() {
                    ev.save = true;
                }
            });
        });
    if !*open {
        // Closed by the × button this frame: drop any open popup with the
        // panel, so a reopen does not resurrect it anchored to a button.
        st.font_popup = None;
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

/// The sample text a font popup previews with.
const FONT_PREVIEW_TEXT: &str = "AaBbGgRr 0123 The quick brown fox";
/// A popup lists at most this many families per pass. Each previewed face
/// reads one font file into the atlas, and a full system can hold hundreds
/// of families; the filter box covers the tail.
const FONT_POPUP_MAX: usize = 300;

/// A font row: a button showing the current description that opens the
/// selection popup. The popup drafts family, style and size against a live
/// preview; only OK writes the Pango-style description ("Family Bold Italic
/// 12") into the config -- a font change rebuilds the whole font system, so
/// it must never fire per keystroke or per list click.
fn font_row(
    ui: &mut egui::Ui,
    label: &str,
    cfg_value: &mut String,
    which: FontWhich,
    st: &mut UiState,
    choices: &[crate::fonts::FontChoice],
    ev: &mut ConfigEvents,
) {
    row(ui, label, |ui| {
        let shown: String = cfg_value.chars().take(30).collect();
        let r = ui.add(egui::Button::new(shown).min_size(egui::vec2(160.0, 0.0)));
        if r.clicked() && st.font_popup.is_none() {
            st.font_popup = Some(seed_popup(which, cfg_value, choices));
        }
        if !st.font_popup.as_ref().is_some_and(|p| p.which == which) {
            return;
        }
        // The list is rebuilt only when one of its inputs changes -- filter
        // text or drafted family -- not every frame.
        {
            let popup = st.font_popup.as_mut().unwrap();
            let needle = popup.search.to_lowercase();
            let stale = !popup
                .list_cache
                .as_ref()
                .is_some_and(|(n, f, _)| *n == needle && *f == popup.family);
            if stale {
                let mut idx: Vec<usize> = choices
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| needle.is_empty() || c.name_lower.contains(&needle))
                    .map(|(i, _)| i)
                    .collect();
                // Drafted family first, then plain alphabetical (a rotate
                // would leave the alphabet split at the draft instead).
                if let Some(p) = idx.iter().position(|&i| choices[i].name == popup.family) {
                    let d = idx.remove(p);
                    idx.insert(0, d);
                }
                idx.truncate(FONT_POPUP_MAX);
                popup.list_cache = Some((needle, popup.family.clone(), idx));
            }
        }
        let mut search = std::mem::take(&mut st.font_popup.as_mut().unwrap().search);
        // Copied out so the popup closure can hold &mut popup while reading
        // it. A few hundred usize once per frame, versus a full
        // filter + sort + String churn before.
        let order: Vec<usize> = st
            .font_popup
            .as_ref()
            .and_then(|p| p.list_cache.as_ref().map(|(_, _, v)| v.clone()))
            .unwrap_or_default();

        let mut commit: Option<String> = None;
        let mut close = false;
        {
            let popup = st.font_popup.as_mut().unwrap();
            let inner = egui::Popup::from_response(&r)
                .open_bool(&mut popup.open)
                .width(420.0)
                // from_response leaves the default left-to-right layout;
                // without this the whole dialog renders as one horizontal
                // strip (found by the layout test below).
                .layout(egui::Layout::top_down_justified(egui::Align::Min))
                // ...and its DEFAULT close behavior is CloseOnClick: ANY
                // click -- including the ones on this popup's own widgets --
                // closed it, so clicking the filter box dismissed the dialog
                // (found by the same test). A dialog closes on outside
                // clicks only.
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| -> Option<String> {
                    // The popup lives in an auto-sizing Area whose available
                    // size is not trustworthy (an earlier version flowed the
                    // whole dialog as one horizontal strip across the
                    // screen). Lay the dialog out in an exactly-sized box
                    // instead: every region gets an explicit rect.
                    ui.allocate_ui_with_layout(
                        egui::vec2(404.0, 452.0),
                        egui::Layout::top_down_justified(egui::Align::Min),
                        |ui| {
                            // ---- Preview (draft face, draft size) ----
                            let face = face_of_idx(choices, popup.family_idx, popup.style);
                            let rt = match face {
                                Some(f) => {
                                    // The terminal draws `size` pt at 96 dpi,
                                    // i.e. size*4/3 px; preview the same
                                    // physical size.
                                    let px = (popup.size_pt * 96.0 / 72.0).clamp(9.0, 48.0);
                                    let key = preview_family_name(&popup.family, f.weight, f.slant);
                                    egui::RichText::new(FONT_PREVIEW_TEXT).font(egui::FontId::new(
                                        px,
                                        egui::FontFamily::Name(std::sync::Arc::from(key.as_str())),
                                    ))
                                }
                                None => egui::RichText::new(FONT_PREVIEW_TEXT)
                                    .weak()
                                    .italics(),
                            };
                            ui.add_sized([404.0, 64.0], egui::Label::new(rt).selectable(false));
                            ui.separator();
                            // ---- Filter ----
                            ui.add_sized(
                                [404.0, 24.0],
                                egui::TextEdit::singleline(&mut search).hint_text("Filter fonts\u{2026}"),
                            );
                            // ---- Family list (neutral labels) | style list ----
                            let mut picked_family: Option<usize> = None;
                            let mut dbl = false;
                            ui.horizontal(|ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(250.0, 240.0),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        egui::ScrollArea::vertical()
                                            .id_salt("font-families")
                                            .auto_shrink([false, false])
                                            .show(ui, |ui| {
                                                for &i in &order {
                                                    let c = &choices[i];
                                                    let resp = ui.selectable_label(
                                                        i == popup.family_idx,
                                                        // The NAME in the neutral UI
                                                        // font: the face belongs to
                                                        // the preview, not the list.
                                                        &c.name,
                                                    );
                                                    if resp.double_clicked() {
                                                        picked_family = Some(i);
                                                        dbl = true;
                                                        break;
                                                    } else if resp.clicked() {
                                                        picked_family = Some(i);
                                                        break;
                                                    }
                                                }
                                            });
                                    },
                                );
                                ui.separator();
                                ui.allocate_ui_with_layout(
                                    egui::vec2(140.0, 240.0),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        egui::ScrollArea::vertical()
                                            .id_salt("font-styles")
                                            .auto_shrink([false, false])
                                            .show(ui, |ui| {
                                                let Some(c) = choices.get(popup.family_idx)
                                                else {
                                                    return;
                                                };
                                                if c.name != popup.family {
                                                    return; // draft names nothing installed
                                                }
                                                for (i, f) in c.faces.iter().enumerate() {
                                                    let words =
                                                        crate::fonts::style_words(f.weight, f.slant);
                                                    let name = if words.is_empty() {
                                                        "Regular".to_string()
                                                    } else {
                                                        words
                                                    };
                                                    if ui
                                                        .selectable_value(&mut popup.style, i, name)
                                                        .double_clicked()
                                                    {
                                                        dbl = true;
                                                        break;
                                                    }
                                                }
                                            });
                                    },
                                );
                            });
                            if let Some(i) = picked_family {
                                // Snap the style to the same weight/slant the
                                // previous family had selected, when offered.
                                let want = face_of_idx(choices, popup.family_idx, popup.style)
                                    .map(|f| (f.weight, f.slant));
                                popup.family = choices[i].name.clone();
                                popup.family_idx = i;
                                popup.style = nearest_style(&choices[i], want);
                            }
                            // ---- Size, OK, Cancel ----
                            // Whole points only: sizes step by one and the
                            // displayed/committed value never carries a
                            // decimal. The number can still be typed, but
                            // dragging is off (speed 0) -- a drag with
                            // fractional speed was how this control used to
                            // mint 11.5-pt fonts.
                            popup.size_pt = popup.size_pt.round();
                            ui.horizontal(|ui| {
                                ui.label("Size");
                                let step = egui::Vec2::new(26.0, 20.0);
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new("\u{2212}").strong(),
                                        )
                                        .min_size(step),
                                    )
                                    .on_hover_text("one point smaller")
                                    .clicked()
                                {
                                    popup.size_pt = (popup.size_pt - 1.0).max(4.0);
                                }
                                ui.add(
                                    egui::DragValue::new(&mut popup.size_pt)
                                        .range(4.0..=96.0)
                                        .speed(0.0)
                                        .fixed_decimals(0)
                                        .suffix(" pt"),
                                );
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new("+").strong(),
                                        )
                                        .min_size(step),
                                    )
                                    .on_hover_text("one point bigger")
                                    .clicked()
                                {
                                    popup.size_pt = (popup.size_pt + 1.0).min(96.0);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("OK").clicked() || dbl {
                                            commit = compose_value(
                                                choices,
                                                &popup.family,
                                                popup.style,
                                                popup.size_pt,
                                            );
                                            close = true;
                                        }
                                        if ui.button("Cancel").clicked() {
                                            close = true;
                                        }
                                    },
                                );
                            });
                        },
                    );
                    commit.clone()
                });
            if commit.is_none() {
                commit = inner.and_then(|i| i.inner);
            }
            if close {
                popup.open = false;
            }
        }
        if let Some(val) = commit {
            if *cfg_value != val {
                *cfg_value = val;
                ev.apply = true;
            }
        }
        st.font_popup.as_mut().unwrap().search = search;
        if !st.font_popup.as_ref().is_some_and(|p| p.open) {
            st.font_popup = None;
        }
    });
}

/// The face a popup draft selects, by cached family index. This is the
/// per-frame accessor; [`face_of`] (by name) is for one-shot paths.
fn face_of_idx<'a>(
    choices: &'a [crate::fonts::FontChoice],
    family_idx: usize,
    style: usize,
) -> Option<&'a crate::fonts::FaceChoice> {
    choices.get(family_idx)?.faces.get(style)
}

/// The drafted face, if the family exists and has one at that index.
fn face_of<'a>(
    choices: &'a [crate::fonts::FontChoice],
    family: &str,
    style: usize,
) -> Option<&'a crate::fonts::FaceChoice> {
    choices.iter().find(|c| c.name == family)?.faces.get(style)
}

/// The style index of the face closest to `want` (weight, slant); defaults
/// to the family's Regular-ish face.
fn nearest_style(
    c: &crate::fonts::FontChoice,
    want: Option<(u16, crate::fonts::DbStyle)>,
) -> usize {
    let (w, s) = want.unwrap_or((400, crate::fonts::DbStyle::Normal));
    c.faces
        .iter()
        .enumerate()
        .min_by_key(|(_, f)| {
            (f.weight as i32 - w as i32).unsigned_abs() + if f.slant == s { 0 } else { 1000 }
        })
        .map_or(0, |(i, _)| i)
}

/// Seed a popup's draft from the current config description: the spec names
/// a family exactly, or it splits into family + style, or the draft starts
/// from its first word so the list at least highlights near it.
fn seed_popup(
    which: FontWhich,
    cfg_value: &str,
    choices: &[crate::fonts::FontChoice],
) -> FontPopup {
    let (spec, size) = crate::config::parse_font(cfg_value);
    let (family, want) = if choices.iter().any(|c| c.name == spec) {
        (spec, None)
    } else if let Some((base, w, s)) = crate::fonts::split_style_suffix(&spec) {
            if choices.iter().any(|c| c.name == base) {
                (base, Some((w, s)))
            } else {
                (spec, None)
            }
        } else {
            (spec, None)
        };
    let fam_idx = choices.iter().position(|c| c.name == family);
    let style = fam_idx.map(|i| nearest_style(&choices[i], want)).unwrap_or(0);
    FontPopup {
        which,
        open: true,
        search: String::new(),
        family,
        family_idx: fam_idx.unwrap_or(usize::MAX),
        style,
        size_pt: size.round(), // whole points; a hand-written 11.5 seeds 12
        list_cache: None,
    }
}

/// Compose the config value the popup commits: "Family Bold Italic 12",
/// with the style words omitted for the plain upright face. `None` when the
/// draft names no real family (OK on a family-less draft must not write
/// garbage).
fn compose_value(
    choices: &[crate::fonts::FontChoice],
    family: &str,
    style: usize,
    size_pt: f32,
) -> Option<String> {
    let face = face_of(choices, family, style)?;
    let words = crate::fonts::style_words(face.weight, face.slant);
    let size = size_pt.round();
    Some(if words.is_empty() {
        format!("{family} {size:.0}")
    } else {
        format!("{family} {words} {size:.0}")
    })
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

    /// One layout pass of the config panel at a given screen size.
    fn pass(ctx: &Context, w: f32, h: f32, open: bool) -> egui::FullOutput {
        let mut st = UiState { font_popup: None };
        let mut cfg = crate::config::Config::default();
        let mut open_local = open;
        let mut full = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(w, h))),
                ..Default::default()
            },
            |ui| { config_window(ui, &mut open_local, &mut cfg, &mut st, &[]); },
        );
        full.textures_delta.clear();
        full
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
        // Open pass: the panel is a full-height frame docked at the right
        // edge (this is what catches a layout that slides it off-screen).
        let full = pass(&ctx, 1280.0, 720.0, true);
        let docked = full.shapes.iter().any(|cs| {
            matches!(
                cs,
                egui::epaint::ClippedShape {
                    shape: egui::Shape::Rect(r),
                    ..
                } if r.rect.size().y > 720.0 * 0.9
                    && (r.rect.max.x - 1280.0).abs() < 1.0
                    && r.rect.size().x > 200.0
            )
        });
        assert!(docked, "panel is not full-height and docked right");
        pass(&ctx, 1280.0, 720.0, true);
        pass(&ctx, 900.0, 600.0, true); // resize
        pass(&ctx, 900.0, 600.0, false); // close
        pass(&ctx, 900.0, 600.0, true); // reopen
    }

    // The font popup: opens from a row, previews installed fonts through the
    // FontPreviewProvider (which reads a real font file into egui's atlas on
    // first use), and stays on screen across passes.
    #[test]
    fn font_popup_layout() {
        let ctx = Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        // A real database so the provider has real files to read (the same
        // construction the app's FontSystem uses).
        let mut db = glyphon::fontdb::Database::new();
        db.load_system_fonts();
        let choices = crate::fonts::font_choices(&db);
        assert!(!choices.is_empty(), "no installed fonts on the test machine");
        // Register the same provider Egui::init does: one entry per face.
        ctx.add_font_provider(std::sync::Arc::new(FontPreviewProvider {
            fonts: std::sync::Mutex::new(
                choices
                    .iter()
                    .flat_map(|c| {
                        c.faces.iter().map(move |f| {
                            (
                                preview_family_name(&c.name, f.weight, f.slant),
                                (f.path.clone(), f.index),
                            )
                        })
                    })
                    .collect(),
            ),
        }));

        let mut st = UiState { font_popup: None };
        let mut cfg = crate::config::Config::default();
        // Seed from a family in the middle of the list: the popup puts the
        // current family first, so the next row down is the first family
        // alphabetically -- two distinct names to measure the list with.
        let mid = &choices[choices.len() / 2];
        cfg.term_font = format!("{} 13", mid.name);
        let mut open = true;
        // The popup, seeded from the config value exactly as the button does.
        st.font_popup = Some(seed_popup(FontWhich::Term, &cfg.term_font, &choices));
        let family = st.font_popup.as_ref().unwrap().family.clone();
        assert_eq!(family, mid.name, "seed should name the family");
        assert_eq!(st.font_popup.as_ref().unwrap().size_pt, 13.0);
        for _ in 0..3 {
            let mut full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_max(
                        egui::pos2(0.0, 0.0),
                        egui::pos2(1280.0, 720.0),
                    )),
                    ..Default::default()
                },
                |ui| { config_window(ui, &mut open, &mut cfg, &mut st, &choices); },
            );
            full.textures_delta.clear();
        }

        // The drafted face made it into egui's font system: the provider
        // read the file and epaint installed it under the face's preview
        // family name when the preview laid out (this is the whole point of
        // the popup). The chain is observable through the family's
        // character map.
        let style = st.font_popup.as_ref().unwrap().style;
        let face = face_of(&choices, &family, style).expect("drafted face exists");
        let key = preview_family_name(&family, face.weight, face.slant);
        let chars = ctx.fonts_mut(|f| {
            f.characters(&egui::FontFamily::Name(std::sync::Arc::from(key.as_str()))).clone()
        });
        assert!(!chars.is_empty(), "preview face was not installed into egui");

        // Layout: the popup must stack VERTICALLY (its default layout is
        // left-to-right, which once rendered the whole dialog as one
        // horizontal strip). Measure: the current family label first, the
        // alphabetically-first family below it, and the OK button under the
        // lists -- all compared by galley position.
        let full = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_max(
                    egui::pos2(0.0, 0.0),
                    egui::pos2(1280.0, 720.0),
                )),
                ..Default::default()
            },
            |ui| { config_window(ui, &mut open, &mut cfg, &mut st, &choices); },
        );
        let label_pos = |want: &str| -> Option<egui::Pos2> {
            full.shapes.iter().find_map(|cs| match cs {
                egui::epaint::ClippedShape { shape: egui::Shape::Text(t), .. }
                    if t.galley.text() == want =>
                {
                    Some(t.pos)
                }
                _ => None,
            })
        };
        let cur = label_pos(&mid.name).expect("current family name in the list");
        let next = label_pos(&choices[0].name).expect("second family name in the list");
        assert!(
            next.y > cur.y + 8.0 && (next.x - cur.x).abs() < 8.0,
            "family list is not a vertical column: {:?} at {cur:?}, {:?} at {next:?}",
            mid.name,
            choices[0].name
        );
        let ok = label_pos("OK").expect("OK button");
        assert!(
            ok.y > cur.y + 100.0,
            "OK button is not below the lists: OK at {ok:?}, list starts at {cur:?}"
        );
        assert!(label_pos("Size").is_some(), "size control missing");

        // Commit round-trip: the composed description parses back to the
        // same family and lands on the same face.
        let composed =
            compose_value(&choices, &family, style, 13.0).expect("draft composes");
        let (spec, size) = crate::config::parse_font(&composed);
        assert_eq!(size, 13.0, "size must round-trip through {composed:?}");
        let r = crate::fonts::resolve_font(&db, &spec);
        assert_eq!(r.family, family, "family must round-trip through {composed:?}");
        assert_eq!(
            (r.weight, r.slant),
            (face.weight, face.slant),
            "style must round-trip through {composed:?}"
        );
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

#[cfg(test)]
mod input_regression {
    use super::*;

    struct Sim<'a> {
        ctx: Context,
        st: &'a mut UiState,
        cfg: &'a mut crate::config::Config,
        open: bool,
        choices: &'a [crate::fonts::FontChoice],
        last_popup_rect: Option<egui::Rect>,
    }

    impl<'a> Sim<'a> {
        fn frame(&mut self, events: Vec<egui::Event>) -> (Vec<String>, bool, Option<String>) {
            let Self { ctx, st, cfg, open, choices, .. } = self;
            let mut full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1280.0, 720.0))),
                    events,
                    ..Default::default()
                },
                |ui| { config_window(ui, open, cfg, st, choices); },
            );
            full.textures_delta.clear();
            let labels: Vec<String> = full
                .shapes
                .iter()
                .filter_map(|cs| match cs {
                    egui::epaint::ClippedShape { shape: egui::Shape::Text(t), .. } => {
                        let s = t.galley.text().to_string();
                        (self.choices.iter().any(|c| c.name == s)).then_some(s)
                    }
                    _ => None,
                })
                .collect();
            if let Some(r) = full.shapes.iter().find_map(|cs| match cs {
                egui::epaint::ClippedShape { shape: egui::Shape::Rect(r), .. } => {
                    let w = r.rect.width();
                    (w > 380.0 && w < 460.0 && r.rect.height() > 18.0 && r.rect.height() < 40.0)
                        .then_some(r.rect)
                }
                _ => None,
            }) {
                self.last_popup_rect = Some(r);
            }
            let popup_open = self.st.font_popup.as_ref().is_some_and(|p| p.open);
            let search = self.st.font_popup.as_ref().map(|p| p.search.clone());
            (labels, popup_open, search)
        }
    }

    #[test]
    /// Clicking and typing in the popup's filter must not dismiss it: the
    /// popup's DEFAULT close behavior (CloseOnClick) closed it on its own
    /// filter click, and the app then tore the state down. Regression test
    /// for the reported "typing into the font filter makes the popup vanish".
    #[test]
    fn filter_click_and_typing_keep_popup_open() {
        let ctx = Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let mut db = glyphon::fontdb::Database::new();
        db.load_system_fonts();
        let choices = crate::fonts::font_choices(&db);
        let mut st = UiState { font_popup: None };
        let mut cfg = crate::config::Config::default();
        let seed_spec = choices[choices.len() / 2].name.clone();
        cfg.term_font = format!("{seed_spec} 13");
        let mut sim = Sim {
            ctx,
            st: &mut st,
            cfg: &mut cfg,
            open: true,
            choices: &choices,
            last_popup_rect: None,
        };

        // pass 0: popup seeded, as the button click does
        sim.st.font_popup = Some(seed_popup(FontWhich::Term, &format!("{seed_spec} 13"), &choices));
        let (l, pop, se) = sim.frame(vec![]);
        eprintln!("P0 popup={pop} labels={} search={se:?}", l.len());
        let (l, pop, se) = sim.frame(vec![]);
        eprintln!("P1 popup={pop} labels={} search={se:?}", l.len());
        let pr = sim.last_popup_rect.expect("filter box laid out in the popup");
        let filter_pos = pr.center();
        let mods = egui::Modifiers::default();
        let (_l, pop, _se) = sim.frame(
            vec![
                egui::Event::PointerMoved(filter_pos),
                egui::Event::PointerButton { pos: filter_pos, button: egui::PointerButton::Primary, pressed: true, modifiers: mods },
                egui::Event::PointerButton { pos: filter_pos, button: egui::PointerButton::Primary, pressed: false, modifiers: mods },
            ],
        );
        let focused = sim.ctx.memory(|m| m.focused().is_some());
        assert!(pop, "click on the popup's own filter dismissed the popup");
        assert!(focused, "the click did not focus the filter box");
        let (l, pop, se) = sim.frame(vec![egui::Event::Text("o".into())]);
        assert!(pop, "typing dismissed the popup");
        assert_eq!(se.as_deref(), Some("o"), "keystroke did not reach the filter");
        let (l2, pop, se) = sim.frame(vec![egui::Event::Text("n".into())]);
        assert!(pop, "typing dismissed the popup");
        assert_eq!(se.as_deref(), Some("on"), "keystroke did not reach the filter");
        assert!(l2.len() <= l.len(), "the filter did not narrow the family list");
    }

    /// The app gates real keystrokes on `egui_winit::consumed`, which is
    /// true whenever egui holds keyboard focus. If focus survives closing
    /// the panel/popup or a click back onto the terminal, every keystroke
    /// afterwards is eaten by egui -- no widget visible, nothing typed:
    /// the reported "letters vanish when I start typing". These cases pin
    /// that focus is actually surrendered.
    fn fresh_sim<'a>(st: &'a mut UiState, cfg: &'a mut crate::config::Config, open: bool, choices: &'a [crate::fonts::FontChoice]) -> Sim<'a> {
        Sim {
            ctx: Context::default(),
            st,
            cfg,
            open,
            choices,
            last_popup_rect: None,
        }
    }

    #[test]
    fn size_selector_is_whole_points_and_easy_to_step() {
        let ctx = Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let mut db = glyphon::fontdb::Database::new();
        db.load_system_fonts();
        let choices = crate::fonts::font_choices(&db);
        let mut st = UiState { font_popup: None };
        let mut cfg = crate::config::Config::default();
        let mid = choices[choices.len() / 2].name.clone();
        // A hand-written decimal in the config must seed a whole point size.
        st.font_popup = Some(seed_popup(FontWhich::Term, &format!("{mid} 12.5"), &choices));
        let seeded = st.font_popup.as_ref().unwrap().size_pt;
        assert_eq!(seeded, 13.0, "12.5 should seed the nearest whole point");
        // A commit never writes a decimal, whatever was drafted.
        let committed = compose_value(&choices, &mid, 0, 12.5).expect("commits");
        assert!(committed.ends_with(" 13"), "commit carries a decimal: {committed}");

        let mut open = true;
        cfg.term_font = format!("{mid} 13");
        let mut texts = |ctx: &Context| -> Vec<String> {
            let mut full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_max(
                        egui::pos2(0.0, 0.0),
                        egui::pos2(1280.0, 720.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    config_window(ui, &mut open, &mut cfg, &mut st, &choices);
                },
            );
            full.textures_delta.clear();
            full.shapes
                .iter()
                .filter_map(|cs| match cs {
                    egui::epaint::ClippedShape { shape: egui::Shape::Text(t), .. } => {
                        Some(t.galley.text().to_string())
                    }
                    _ => None,
                })
                .collect()
        };
        let g = {
            texts(&ctx);
            texts(&ctx)
        };
        // egui renders a DragValue's number and suffix as separate galleys.
        assert!(g.iter().any(|t| t.trim() == "13"), "no whole-point size label");
        assert!(g.iter().any(|t| t.trim() == "pt"), "size label lost its unit");
        assert!(
            !g.iter().any(|t| t.trim() == "12.5" || t.trim() == "13.5"),
            "a fractional size is on screen: {g:?}"
        );
        // One-click steppers flank the number.
        assert!(g.iter().any(|t| t.trim() == "\u{2212}"), "no minus stepper");
        assert!(g.iter().any(|t| t.trim() == "+"), "no plus stepper");
    }

    #[test]
    fn keyboard_focus_released_when_ui_steps_aside() {
        let mut db = glyphon::fontdb::Database::new();
        db.load_system_fonts();
        let choices = crate::fonts::font_choices(&db);
        let seed_spec = choices[choices.len() / 2].name.clone();
        let mods = egui::Modifiers::default();

        // Focus the font filter: click it, verify focus actually landed.
        let focus_filter = |sim: &mut Sim| {
            let p = seed_popup(FontWhich::Term, &format!("{seed_spec} 13"), sim.choices);
            sim.st.font_popup = Some(p);
            sim.frame(vec![]);
            let r = sim.last_popup_rect.expect("filter laid out");
            let pos = r.center();
            sim.frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: mods },
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: mods },
            ]);
            assert!(sim.ctx.memory(|m| m.focused().is_some()), "filter never got focus");
        };

        // A: panel closed (Save / x) while the filter had focus. Closing the
        // panel also clears the popup, exactly as config_window does, so the
        // app's edge helper sees (true,true) -> (false,false).
        let mut st = UiState { font_popup: None };
        let mut cfg = crate::config::Config::default();
        let mut sim = fresh_sim(&mut st, &mut cfg, true, &choices);
        focus_filter(&mut sim);
        let mut prev = (true, true);
        eprintln!("DBG pre-close: mem={:?} i.focused={}", sim.ctx.memory(|m| m.focused()), sim.ctx.input(|i| i.focused));
        sim.open = false;
        sim.st.font_popup = None;
        sim.frame(vec![]);
        eprintln!("DBG post-close-frame: mem={:?} i.focused={}", sim.ctx.memory(|m| m.focused()), sim.ctx.input(|i| i.focused));
        release_focus_if_closed(&sim.ctx, false, false, &mut prev);
        sim.frame(vec![egui::Event::Text("z".into())]);
        eprintln!("DBG post-z: mem={:?} i.focused={}", sim.ctx.memory(|m| m.focused()), sim.ctx.input(|i| i.focused));
        assert!(sim.ctx.memory(|m| m.focused()).is_none(), "egui still holds a widget focus after the panel closed");

        // B: popup closed (OK/Cancel) while the filter had focus, panel open.
        let mut st = UiState { font_popup: None };
        let mut cfg = crate::config::Config::default();
        let mut sim = fresh_sim(&mut st, &mut cfg, true, &choices);
        focus_filter(&mut sim);
        let mut prev = (true, true);
        sim.st.font_popup = None;
        sim.frame(vec![]);
        release_focus_if_closed(&sim.ctx, true, false, &mut prev);
        sim.frame(vec![egui::Event::Text("z".into())]);
        assert!(sim.ctx.memory(|m| m.focused()).is_none(), "egui still holds a widget focus after the popup closed");

        // C: filter focused, user clicks the terminal. Such a click is NOT
        // egui-consumed, and the app releases focus for every click it
        // claims itself; simulate that call.
        let mut st = UiState { font_popup: None };
        let mut cfg = crate::config::Config::default();
        let mut sim = fresh_sim(&mut st, &mut cfg, true, &choices);
        focus_filter(&mut sim);
        let term = egui::pos2(400.0, 600.0); // left of the right-docked panel
        sim.frame(vec![
            egui::Event::PointerMoved(term),
            egui::Event::PointerButton { pos: term, button: egui::PointerButton::Primary, pressed: true, modifiers: mods },
            egui::Event::PointerButton { pos: term, button: egui::PointerButton::Primary, pressed: false, modifiers: mods },
        ]);
        eprintln!("DBG C pre-release: mem={:?}", sim.ctx.memory(|m| m.focused()));
        release_focus(&sim.ctx);
        eprintln!("DBG C post-release: mem={:?}", sim.ctx.memory(|m| m.focused()));
        sim.frame(vec![egui::Event::Text("z".into())]);
        eprintln!("DBG C post-z: mem={:?}", sim.ctx.memory(|m| m.focused()));
        assert!(sim.ctx.memory(|m| m.focused()).is_none(), "egui kept keyboard focus after a click on the terminal");
    }
}
