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

// Card geometry. The text column is fixed and the thumbnail adds its own
// width, so a card's width is a pure function of it (see `card_width`):
// that is what makes the row centring computable.
const CARD_PAD: f32 = 10.0;
const TEXT_COL: f32 = 96.0;
const CARD_TXT_GAP: f32 = 8.0;
/// The thumbnail's height; its width follows the terminal's aspect.
pub const THUMB_H: u32 = 52;
pub const THUMB_W_RANGE: (u32, u32) = (40, 150);
/// Minimum card height when there is no thumbnail (three text lines).
const CARD_MIN_H: f32 = 56.0;
const CARD_GAP: f32 = 12.0;

/// A card's exact width: pads plus the text column, plus the thumbnail when
/// present.
fn card_width(card: &SwitcherCard) -> f32 {
    let mut w = CARD_PAD * 2.0 + TEXT_COL;
    if let Some((_, (tw, _))) = &card.thumb {
        w += *tw as f32 + CARD_TXT_GAP;
    }
    w
}

/// A card's exact height: the thumbnail (tallest element) or the text
/// lines, plus pads.
fn card_height(card: &SwitcherCard) -> f32 {
    let th = card.thumb.as_ref().map_or(0.0, |(_, (_, th))| *th as f32);
    th.max(CARD_MIN_H) + CARD_PAD * 2.0
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
            // Claim the whole backdrop (see docs).
            ui.allocate_rect(ui.max_rect(), egui::Sense::click());
            let avail_w = ui.available_width();
            let widths: Vec<f32> = cards.iter().map(card_width).collect();
            // Greedy fill: which cards land on which row, and each row's
            // exact width.
            let mut rows: Vec<Vec<f32>> = vec![Vec::new()];
            for w in &widths {
                let gap = if rows.last().unwrap().is_empty() {
                    0.0
                } else {
                    CARD_GAP
                };
                let cur: f32 = rows.last().unwrap().iter().sum::<f32>() + gap + w;
                if cur > avail_w && !rows.last().unwrap().is_empty() {
                    rows.push(Vec::new());
                }
                rows.last_mut().unwrap().push(*w);
            }
            let row_w: Vec<f32> = rows
                .iter()
                .map(|r| r.iter().sum::<f32>() + (r.len().saturating_sub(1)) as f32 * CARD_GAP)
                .collect();
            let max_h = cards.iter().map(card_height).fold(0.0, f32::max);
            let block_h = rows.len() as f32 * max_h + (rows.len() - 1) as f32 * CARD_GAP;
            ui.add_space(((ui.available_height() - block_h) / 2.0).max(24.0));
            for (r, w) in row_w.iter().enumerate() {
                if r > 0 {
                    ui.add_space(CARD_GAP);
                }
                // Every row (not just the first) is centred on its own width.
                ui.add_space((avail_w - w).max(0.0) / 2.0);
                ui.scope(|ui| {
                    ui.set_max_width(*w);
                    ui.horizontal(|ui| {
                        let mut first = true;
                        for (i, card) in cards.iter().enumerate() {
                            if card_index_row(i, &widths, avail_w) != r {
                                continue;
                            }
                            if !first {
                                ui.add_space(CARD_GAP);
                            }
                            first = false;
                            card_button(ui, i, card, i == selected, accent, pick);
                        }
                    });
                });
            }
        });
}

/// Which row (0-based) card `i` lands on under the same greedy fill
/// `switcher_screen` uses.
fn card_index_row(i: usize, widths: &[f32], avail_w: f32) -> usize {
    let mut x = 0.0f32;
    let mut row = 0usize;
    for (j, w) in widths.iter().enumerate() {
        if x + w > avail_w && x > 0.0 {
            row += 1;
            x = 0.0;
        }
        if j == i {
            return row;
        }
        x += w + CARD_GAP;
    }
    row
}

/// One tab card: a rounded frame with the live thumbnail (when read back)
/// and the title, "tab N" and the working directory. The selected card
/// gets the accent stroke and the brightest fill.
fn card_button(
    ui: &mut egui::Ui,
    index: usize,
    card: &SwitcherCard,
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
    let frame = egui::Frame::NONE
        .fill(fill)
        .corner_radius(10.0)
        .stroke(egui::Stroke::new(1.5, stroke))
        .inner_margin(egui::Margin::symmetric(
            CARD_PAD as i8,
            CARD_PAD as i8,
        ));
    let r = frame.show(ui, |ui| {
        ui.set_min_size(egui::vec2(card_width(card), card_height(card) - CARD_PAD * 2.0));
        if card.thumb.is_some() {
            ui.horizontal_centered(|ui| {
                if let Some((handle, _)) = &card.thumb {
                    // Native texture size is exactly the thumbnail size.
                    ui.image(egui::load::SizedTexture::from(handle));
                    ui.add_space(CARD_TXT_GAP);
                }
                text_column(ui, index, card, selected);
            });
        } else {
            ui.vertical_centered(|ui| {
                text_column(ui, index, card, selected);
            });
        }
    });
    if r.response.clicked() {
        *pick = Some(index);
    }
}

/// The card's text: the title (entangled tabs keep the `*` marker, selected
/// is bold), a "tab N" caption, and the working directory when present. All
/// of it truncates to the fixed text column rather than growing the card.
fn text_column(
    ui: &mut egui::Ui,
    index: usize,
    card: &SwitcherCard,
    selected: bool,
) {
    ui.set_max_width(TEXT_COL);
    let mut title = if card.marker {
        format!("* {}", card.title)
    } else {
        card.title.clone()
    };
    if title.is_empty() {
        title = format!("tab {}", index + 1);
    }
    let mut rt = egui::RichText::new(title);
    if selected {
        rt = rt.strong();
    }
    ui.add(egui::Label::new(rt).wrap_mode(egui::TextWrapMode::Truncate));
    ui.label(egui::RichText::new(format!("tab {}", index + 1)).weak().small());
    if let Some(cwd) = &card.cwd {
        ui.add(
            egui::Label::new(
                egui::RichText::new(cwd)
                    .weak()
                    .small()
                    .color(egui::Color32::from_rgb(98, 102, 120)),
            )
            .wrap_mode(egui::TextWrapMode::Truncate),
        );
    }
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
        let tex = ctx.load_texture(
            "test-thumb",
            egui::ColorImage::from_rgba_unmultiplied([64, 52], &[7u8, 9, 11, 255].repeat(64 * 52)),
            egui::TextureOptions::LINEAR,
        );
        let cards: Vec<SwitcherCard> = (0..8)
            .map(|i| SwitcherCard {
                title: format!("a much longer tab title number {i}"),
                active: i == 1,
                marker: i == 3,
                cwd: if i % 2 == 0 { Some(format!("~/projects/x{i}")) } else { None },
                thumb: if i < 3 { Some((tex.clone(), (64, 52))) } else { None },
            })
            .collect();
        // Wide screen: one row. Narrow screen: several centred rows.
        for (w, h) in [(1280.0, 720.0), (560.0, 480.0)] {
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
                full.textures_delta.clear();
            }
        }
    }
}

// scratch test appended temporarily
