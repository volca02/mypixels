//! egui integration for the tab strip, sidebars, switcher and transient UI.
//! The terminal uses its own pipeline; egui is composited over it in the same
//! render pass. Shared styling lives in `ui_style`, and tab layout in `tab_bar`.
//! Sidebars stay passive until an editor is explicitly focused; modal pickers
//! retain their own keyboard behavior. The app routes terminal keys directly.

use egui::{Context, FullOutput, ViewportId};
use crate::ui_style::{TermStyle, Icon, text, icon as draw_icon, icon_button, actions, section, form_grid, rgb_c32};

/// The egui side of the app: context, winit event state and the wgpu
/// renderer that paints egui's tessellation.
pub struct Egui {
    pub ctx: Context,
    pub state: egui_winit::State,
    pub renderer: egui_wgpu::Renderer,
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
#[derive(Default)]
pub struct UiState {
    /// Frames the context menu has been on screen. The right-press that
    /// *opened* the menu is still in the input state of the first frame
    /// that draws it - dismissal must not see its own opening.
    pub menu_age: u32,
    /// The open font popup, if any (at most one at a time).
    pub font_popup: Option<FontPopup>,
    /// Theme names for the picker. Fetched lazily and re-fetched after
    /// save/revert -- dropping a new file into a themes dir is exactly when
    /// the user expects the list to have noticed.
    pub theme_choice: Vec<String>,
    pub theme_fetched: bool,
    pub settings_page: SettingsPage,
    /// A single slot keeps the right-side surfaces mutually exclusive.
    pub right_panel: RightPanel,
    /// Focus from the last completed UI frame, for editors opened by a click.
    sidebar_focus: Option<egui::Id>,
    /// Top of the terminal below the tab strip, in logical points.
    pub panel_top: f32,
    /// The tab-rename box, egui's version of the original's GTK entry. The
    /// app mirrors this to decide whether to swallow keys.
    pub rename_open: bool,
    pub rename_draft: String,
    /// Screen position (logical points) chosen by the app when it opens the
    /// box -- just under the tab strip, where row 0 used to host the prompt.
    pub rename_pos: [f32; 2],
    /// True for the frame the box opens: focus the entry and pre-select the
    /// draft, so the first typed key replaces it exactly like selected text
    /// in the GTK original (Enter with nothing typed keeps it).
    pub rename_seed: bool,
    /// Set by `rename_box` when the user commits (Some(Some(_))) or cancels
    /// (Some(None)); the app applies and clears it after the frame.
    pub rename_out: Option<Option<String>>,
    /// A font row toggled its popup closed on button-press; the matching
    /// release must not count as a fresh open click (see `font_row`).
    font_suppress: bool,
    /// The send-tab-to-window picker (Alt+Shift+w). Items and targets are
    /// filled by the app when it opens the box; a click reports the target
    /// window index, usize::MAX = "detach into a new window".
    pub move_open: bool,
    pub move_items: Vec<String>,
    pub move_targets: Vec<usize>,
    pub move_pos: [f32; 2],
    /// Some(Some(t)) = move to target t, Some(None) = cancelled, None =
    /// still open. Same shape as `rename_out` for the same reason: the
    /// app must learn about dismissal, not only about choice.
    pub move_out: Option<Option<usize>>,
    /// The right-click context menu. Rows are (label, action, enabled);
    /// `menu_seps` are the indices a separator goes BEFORE. Actions are the
    /// app's own `do_action` names, so the menu adds no new machinery.
    pub menu_open: bool,
    pub menu_items: Vec<(String, String, bool)>,
    pub menu_seps: Vec<usize>,
    pub menu_pos: [f32; 2],
    /// Some(Some(action)) = run this, Some(None) = dismissed, None = open.
    pub menu_out: Option<Option<String>>,
    /// The command-history panel (Alt+Shift+H): the active tab's OSC 133
    /// pool as a live right-side list with filters, search, and actions.
    /// `history_rows` is the app's snapshot (rebuilt every frame while
    /// open); `history_out` is a one-shot (seq, action) the app applies
    /// after the frame, the same contract as the other `*_out` flags.
    pub history_rows: Vec<HistoryRow>,
    pub history_filter: HistoryFilter,
    pub history_query: String,
    pub history_search_open: bool,
    pub history_oldest_first: bool,
    pub history_sel: Option<u64>,
    pub history_out: Option<(u64, String)>,
}

/// A window has one sidebar slot. Adding a panel here keeps visibility exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RightPanel {
    #[default]
    Closed,
    History,
    Settings,
}

impl UiState {
    /// Sidebars are passive until an editor is explicitly focused. Merely
    /// opening a panel or clicking one of its buttons never claims typing.
    pub fn owns_keyboard(&self, ctx: &egui::Context) -> bool {
        self.rename_open || self.menu_open || self.move_open
            || (self.right_panel != RightPanel::Closed && ctx.memory(|m| m.focused())
                .is_some_and(|id| egui::text_edit::TextEditState::load(ctx, id).is_some()))
    }

    /// Button focus must not turn a passive sidebar into a keyboard surface.
    /// IME output identifies a currently rendered editor, including DragValue.
    pub fn release_passive_focus(&mut self, ctx: &egui::Context, output: &egui::PlatformOutput) {
        let focus = ctx.memory(|m| m.focused());
        let pending_editor = focus != self.sidebar_focus
            && focus.is_some_and(|id| egui::text_edit::TextEditState::load(ctx, id).is_some());
        self.sidebar_focus = focus;
        if self.right_panel != RightPanel::Closed && !self.rename_open && !self.menu_open && !self.move_open
            && output.ime.is_none() {
            // DragValue and history search request edit focus after layout;
            // allow the next frame to render their editor before deciding.
            if pending_editor {
                ctx.request_repaint();
            } else {
                release_focus(ctx);
                self.sidebar_focus = None;
            }
        }
    }

    pub fn panel_is(&self, panel: RightPanel) -> bool {
        self.right_panel == panel
    }

    pub fn toggle_panel(&mut self, panel: RightPanel) {
        self.right_panel = if self.panel_is(panel) { RightPanel::Closed } else { panel };
        self.font_popup = None;
    }

    pub fn close_panel(&mut self, panel: RightPanel) {
        if self.panel_is(panel) {
            self.right_panel = RightPanel::Closed;
            self.font_popup = None;
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SettingsPage {
    #[default]
    Appearance,
    Window,
    Tabs,
    Terminal,
}

/// The history panel's list filter, in chip order.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryFilter {
    #[default]
    All,
    Success,
    Error,
}

/// One command in the history panel. Every field is precomputed by the
/// app (at pool close, refreshed every frame while the panel is open),
/// so the panel - which is drawn on every frame of the app - never
/// touches the pool, the scrollback grid, or a codec.
#[derive(Clone)]
pub struct HistoryRow {
    pub seq: u64,
    /// Input captured at C, without the prompt. Can be missing when the
    /// shell does not mark its input or that input has already been lost.
    pub cmd: String,
    pub exit: Option<i32>,
    pub done: bool,
    pub alt_seen: bool,
    pub lines: usize,
    /// Local clock of the D mark (the C mark's while running): 11:21:03.
    pub time: String,
    /// 412 ms / 1.8 s / 2 m 05 s - empty while running.
    pub duration: String,
}

impl HistoryRow {
    fn command_label(&self) -> &str {
        if !self.cmd.is_empty() {
            &self.cmd
        } else if self.done {
            "(command unavailable)"
        } else {
            "(running)"
        }
    }
}

/// What the filter chip and the search box keep visible. Pure, so the
/// panel's behaviour is testable without a window.
pub fn history_visible(row: &HistoryRow, filter: HistoryFilter, query: &str) -> bool {
    match filter {
        HistoryFilter::All => {}
        HistoryFilter::Success => {
            if row.exit.is_some_and(|e| e != 0) {
                return false;
            }
        }
        HistoryFilter::Error => {
            if !row.exit.is_some_and(|e| e != 0) {
                return false;
            }
        }
    }
    if query.is_empty() {
        return true;
    }
    row.cmd.to_lowercase().contains(&query.to_lowercase())
}

/// The context menu: a borderless popup at the pointer, one button per row.
/// Same open/click-away/Escape contract as the rename box and the window
/// picker -- the three small popups should behave identically or they lie
/// about being the same kind of thing.
pub fn ctx_menu(ui: &egui::Ui, st: &mut UiState) {
    if !st.menu_open {
        st.menu_out = None;
        return;
    }
    let ctx = ui.ctx();
    let pos = egui::pos2(st.menu_pos[0], st.menu_pos[1]);
    let mut out: Option<Option<String>> = None;
    let mut area = egui::Rect::NOTHING;
    egui::Window::new("ctx_menu")
        .id(egui::Id::new("ctx_menu_box"))
        .fade_in(false)
        .fade_out(false)
        .title_bar(false)
        .movable(false)
        .resizable(false)
        .fixed_pos(pos)
        .constrain(true)
        .show(&ctx, |ui| {
            ui.set_width(210.0);
            for (i, (label, action, enabled)) in st.menu_items.iter().enumerate() {
                if st.menu_seps.contains(&i) {
                    ui.separator();
                }
                let resp = ui.add_enabled(*enabled, egui::Button::new(label).min_size(egui::vec2(198.0, 0.0)));
                if resp.clicked() {
                    out = Some(Some(action.clone()));
                }
            }
            area = ui.response().rect;
        });
    // One frame of grace: the opening press (and any key in the same batch)
    // arrives in the input state of the first frame that draws the menu.
    if out.is_none()
        && st.menu_age >= 2
        && ctx.input(|i| {
            i.key_pressed(egui::Key::Escape)
                || (i.pointer.any_pressed()
                    && i.pointer.interact_pos().is_some_and(|p| !area.expand(4.0).contains(p)))
        })
    {
        out = Some(None);
    }
    st.menu_age += 1;
    st.menu_out = out;
}

/// The window picker for "send tab to window": one small frameless list,
/// every live window plus an escape hatch into a new one. Click elsewhere
/// or Escape and nothing happens, exactly like the rename box.
pub fn move_pick_box(ui: &egui::Ui, st: &mut UiState) {
    if !st.move_open {
        st.move_out = None;
        return;
    }
    let ctx = ui.ctx();
    let pos = egui::pos2(st.move_pos[0], st.move_pos[1]);
    let mut out: Option<Option<usize>> = None;
    let mut area = egui::Rect::NOTHING;
    egui::Window::new("move_tab_pick")
        .id(egui::Id::new("move_tab_pick_box"))
        .title_bar(false)
        .movable(false)
        .resizable(false)
        .fixed_pos(pos)
        .constrain(false)
        .show(&ctx, |ui| {
            ui.label("Send tab to:");
            for (i, label) in st.move_items.iter().enumerate() {
                if ui.button(label).clicked() {
                    out = Some(Some(st.move_targets[i]));
                }
            }
            if ui.button("Cancel").clicked() {
                out = Some(None);
            }
            area = ui.response().rect;
        });
    if out.is_none()
        && ctx.input(|i| {
            i.key_pressed(egui::Key::Escape)
                || (i.pointer.any_pressed()
                    && i.pointer.interact_pos().is_some_and(|p| !area.expand(8.0).contains(p)))
        })
    {
        out = Some(None);
    }
    st.move_out = out;
}

/// egui-winit's consumption flag uses the pointer from the last UI pass.
/// Hit-test the event's current position when a sidebar is the only surface.
pub fn panel_pointer_input(ctx: &egui::Context, at: egui::Pos2) -> bool {
    ctx.egui_is_using_pointer()
        || ctx.layer_id_at(at).is_some_and(|layer| layer.order != egui::Order::Background)
}

/// The history sidebar uses its own compact layout, while retaining the
/// terminal theme's background, foreground, accent, and status colours.
pub fn history_panel(ui: &mut egui::Ui, st: &mut UiState, ok: egui::Color32, err: egui::Color32, top: f32) {
    if !st.panel_is(RightPanel::History) {
        st.history_out = None;
        return;
    }
    st.history_out = None;
    if !st.menu_open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        release_focus(ui.ctx());
    }
    let colors = TermStyle::new(ui, ok, err);
    let TermStyle { bg, fg, muted, border, accent, card, .. } = colors;
    let mut host = ui.max_rect();
    host.min.y = host.min.y.max(top);
    host.min.x = (host.max.x - 380.0).max(host.min.x);
    // Drawing a Panel in a root-ui scope advances the root cursor past
    // the full-height sidebar. egui then claims the entire background as
    // used, including the terminal. A separate Area leaves it available.
    egui::Area::new(egui::Id::unique("mypixels_history_area"))
        .fixed_pos(host.min)
        .order(egui::Order::Middle)
        .fade_in(false)
        .movable(false)
        .constrain(false)
        .show(ui.ctx(), |ui| {
            ui.set_max_size(host.size());
            egui::Frame::new().fill(bg).inner_margin(18.0).show(ui, |ui| {
                ui.set_width((host.width() - 36.0).max(0.0));
                ui.set_min_height((host.height() - 36.0).max(0.0));
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                let edge = ui.max_rect().left() - 18.0;
                ui.painter().vline(edge, host.y_range(), egui::Stroke::new(1.0, border));
                let (header, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::hover());
                draw_icon(
                    ui.painter(),
                    Icon::Layers,
                    egui::pos2(header.left() + 14.0, header.center().y),
                    fg,
                );
                text(
                    ui,
                    egui::Rect::from_min_max(
                        egui::pos2(header.left() + 36.0, header.top()),
                        egui::pos2(header.right() - 68.0, header.bottom()),
                    ),
                    "Command History",
                    fg,
                    16.0,
                    true,
                );
                let search_rect = egui::Rect::from_center_size(
                    egui::pos2(header.right() - 46.0, header.center().y),
                    egui::vec2(30.0, 30.0),
                );
                let search = icon_button(ui, search_rect, Icon::Search, "Search commands", &colors);
                let focus_search = search.clicked();
                if focus_search {
                    st.history_search_open = !st.history_search_open;
                    if !st.history_search_open {
                        st.history_query.clear();
                    }
                }
                let more_rect = egui::Rect::from_center_size(
                    egui::pos2(header.right() - 10.0, header.center().y),
                    egui::vec2(30.0, 30.0),
                );
                let more = icon_button(ui, more_rect, Icon::More, "History options", &colors);
                egui::Popup::from_toggle_button_response(&more)
                    .layout(egui::Layout::top_down_justified(egui::Align::Min))
                    .show(|ui| {
                        if ui.button("Close history").clicked() {
                            st.close_panel(RightPanel::History);
                        }
                    });
                ui.add_space(16.0);
                if st.history_search_open {
                    let search = ui.add_sized(
                        [ui.available_width(), 32.0],
                        egui::TextEdit::singleline(&mut st.history_query)
                            .hint_text("Search commands…")
                            .font(egui::FontId::monospace(14.0))
                            .margin(egui::vec2(10.0, 8.0)),
                    );
                    if focus_search {
                        search.request_focus();
                    }
                    ui.add_space(12.0);
                }
                let (filters, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::hover());
                let mut x = filters.left() + 2.0;
                for (filter, label, icon, width) in [
                    (HistoryFilter::All, "All", None, 54.0),
                    (HistoryFilter::Success, "Success", Some(Icon::Check), 102.0),
                    (HistoryFilter::Error, "Error", Some(Icon::Error), 82.0),
                ] {
                    let rect = egui::Rect::from_min_size(egui::pos2(x, filters.top()), egui::vec2(width, 34.0));
                    let response = ui.interact(rect, ui.scope_id().with(label), egui::Sense::click());
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(egui::Role::RadioButton, true, st.history_filter == filter, label)
                    });
                    if response.clicked() {
                        st.history_filter = filter;
                    }
                    let selected = st.history_filter == filter;
                    let fill = if selected {
                        bg.lerp_to_gamma(accent, 0.16)
                    } else if response.hovered() {
                        bg.lerp_to_gamma(fg, 0.07)
                    } else {
                        card
                    };
                    ui.painter().rect(
                        rect,
                        17.0,
                        fill,
                        egui::Stroke::new(1.0, if selected || response.has_focus() { accent } else { border }),
                        egui::StrokeKind::Inside,
                    );
                    if let Some(icon) = icon {
                        draw_icon(
                            ui.painter(),
                            icon,
                            egui::pos2(x + 19.0, rect.center().y),
                            if selected {
                                accent
                            } else if filter == HistoryFilter::Success {
                                ok
                            } else {
                                muted
                            },
                        );
                    }
                    let label_rect =
                        if icon.is_some() { rect.with_min_x(x + 31.0) } else { rect.shrink2(egui::vec2(14.0, 0.0)) };
                    text(ui, label_rect, label, if selected { accent } else { muted }, 14.0, false);
                    x += width + 7.0;
                }
                let clock_rect = egui::Rect::from_min_size(egui::pos2(x, filters.top()), egui::vec2(42.0, 34.0));
                ui.painter().rect(
                    clock_rect,
                    17.0,
                    card,
                    egui::Stroke::new(1.0, if st.history_oldest_first { accent } else { border }),
                    egui::StrokeKind::Inside,
                );
                let clock = icon_button(
                    ui,
                    clock_rect,
                    Icon::Clock,
                    if st.history_oldest_first { "Newest first" } else { "Oldest first" },
                    &colors,
                );
                if clock.clicked() {
                    st.history_oldest_first = !st.history_oldest_first;
                }
                ui.add_space(18.0);
                // The list scrolls independently; on short windows the outer
                // scroll area keeps every details action reachable as well.
                let list_height = (ui.available_height() - 390.0).clamp(76.0, 274.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("history_body").show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("history_commands")
                        .auto_shrink([false, true])
                        .max_height(list_height)
                        .show(ui, |ui| {
                            let rows: Vec<_> = st
                                .history_rows
                                .iter()
                                .filter(|r| history_visible(r, st.history_filter, &st.history_query))
                                .collect();
                            // Keep the details card tied to a visible row after
                            // filtering, searching, tab changes, or pool eviction.
                            if !rows.iter().any(|r| Some(r.seq) == st.history_sel) {
                                st.history_sel = rows.iter().find(|r| r.done).or(rows.first()).map(|r| r.seq);
                            }
                            if rows.is_empty() {
                                ui.add_space(14.0);
                                ui.label(
                                    egui::RichText::new(if st.history_rows.is_empty() {
                                        "No command history yet"
                                    } else {
                                        "No commands match"
                                    })
                                    .color(muted),
                                );
                                ui.add_space(14.0);
                            }
                            let seq_width =
                                rows.iter().map(|r| r.seq.to_string().len()).max().unwrap_or(2).max(2) as f32 * 8.5
                                    + 12.0;
                            for index in 0..rows.len() {
                                let row = rows[if st.history_oldest_first { rows.len() - 1 - index } else { index }];
                                let (rect, _) = ui
                                    .allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::hover());
                                let response = ui.interact(
                                    rect,
                                    ui.scope_id().with(("history_row", row.seq)),
                                    egui::Sense::click(),
                                );
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::Role::RadioButton,
                                        true,
                                        st.history_sel == Some(row.seq),
                                        &row.cmd,
                                    )
                                });
                                if response.clicked() {
                                    st.history_sel = Some(row.seq);
                                }
                                let dot = egui::pos2(rect.left() + 15.0, rect.center().y);
                                ui.painter().vline(
                                    dot.x,
                                    rect.y_range(),
                                    egui::Stroke::new(1.0, bg.lerp_to_gamma(ok, 0.55)),
                                );
                                if st.history_sel == Some(row.seq) || response.hovered() || response.has_focus() {
                                    let selected = st.history_sel == Some(row.seq) || response.has_focus();
                                    ui.painter().rect(
                                        rect.shrink2(egui::vec2(0.0, 1.0)),
                                        7.0,
                                        bg.lerp_to_gamma(accent, if selected { 0.13 } else { 0.045 }),
                                        egui::Stroke::new(1.0, if selected { accent } else { border }),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                let status = colors.status(row.done, row.exit);
                                ui.painter().circle_filled(dot, 7.5, bg);
                                ui.painter().circle_filled(dot, 5.5, status);
                                if row.exit.is_some_and(|e| e != 0) {
                                    draw_icon(ui.painter(), Icon::Close, dot, bg);
                                } else if !row.done {
                                    ui.painter().circle_stroke(dot, 3.0, egui::Stroke::new(1.0, bg));
                                }
                                text(
                                    ui,
                                    rect.with_min_x(rect.left() + 38.0).with_max_x(rect.left() + 38.0 + seq_width),
                                    &format!("#{}", row.seq),
                                    muted,
                                    14.0,
                                    false,
                                );
                                let cmd_x = rect.left() + 38.0 + seq_width;
                                if row.done {
                                    draw_icon(
                                        ui.painter(),
                                        if row.exit.is_some_and(|e| e != 0) {
                                            Icon::Close
                                        } else {
                                            Icon::Check
                                        },
                                        egui::pos2(cmd_x + 4.0, rect.center().y),
                                        status,
                                    );
                                }
                                text(
                                    ui,
                                    rect.with_min_x(cmd_x + 15.0).with_max_x(rect.right() - 59.0),
                                    row.command_label(),
                                    fg,
                                    14.0,
                                    false,
                                );
                                text(
                                    ui,
                                    rect.with_min_x(rect.right() - 50.0),
                                    row.time.get(..5).unwrap_or(&row.time),
                                    muted,
                                    14.0,
                                    false,
                                );
                                response.on_hover_text(&row.cmd).on_hover_cursor(egui::CursorIcon::PointingHand);
                            }
                        });
                    if let Some(row) = st.history_rows.iter().find(|r| Some(r.seq) == st.history_sel) {
                        ui.add_space(18.0);
                        if let Some(action) = history_details(ui, row, &colors) {
                            st.history_out = Some((row.seq, action.to_string()));
                        }
                    }
                });
            });
    });
}

fn history_details(ui: &mut egui::Ui, row: &HistoryRow, c: &TermStyle) -> Option<&'static str> {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 368.0), egui::Sense::hover());
    ui.painter().rect(rect, 9.0, c.card, egui::Stroke::new(1.0, c.border), egui::StrokeKind::Inside);
    let left = rect.left() + 13.0;
    let title = egui::Rect::from_min_max(
        egui::pos2(left, rect.top() + 6.0),
        egui::pos2(rect.right() - 12.0, rect.top() + 46.0),
    );
    text(
        ui,
        title,
        &format!("#{}  {}", row.seq, row.command_label()),
        c.fg,
        15.0,
        true,
    );
    ui.interact(title, ui.scope_id().with("history_title"), egui::Sense::hover()).on_hover_text(&row.cmd);
    let status = if !row.done {
        "running".to_string()
    } else {
        match row.exit {
            Some(0) => "success (exit 0)".to_string(),
            Some(e) => format!("failed (exit {e})"),
            None => "finished (no status)".to_string(),
        }
    };
    let output = if row.alt_seen { "full-screen program".to_string() } else { format!("{} lines", row.lines) };
    for (i, (label, value)) in [
        ("Status", status.as_str()),
        ("Duration", if row.duration.is_empty() { "—" } else { &row.duration }),
        ("Output", output.as_str()),
        ("Time", row.time.as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        let y = rect.top() + 56.0 + i as f32 * 26.0;
        let line = egui::Rect::from_min_size(egui::pos2(left, y), egui::vec2(rect.width() - 26.0, 26.0));
        text(ui, line.with_max_x(left + 100.0), label, c.muted, 14.0, false);
        let value_x = left + 103.0;
        if i == 0 {
            draw_icon(
                ui.painter(),
                if !row.done {
                    Icon::Clock
                } else if row.exit.is_some_and(|e| e != 0) {
                    Icon::Error
                } else {
                    Icon::Check
                },
                egui::pos2(value_x + 6.0, line.center().y),
                c.status(row.done, row.exit),
            );
        }
        text(ui, line.with_min_x(value_x + if i == 0 { 22.0 } else { 0.0 }), value, c.fg, 14.0, false);
    }
    let divider = rect.top() + 170.0;
    ui.painter().hline(rect.x_range(), divider, egui::Stroke::new(1.0, c.border));
    text(
        ui,
        egui::Rect::from_min_size(egui::pos2(left, divider + 8.0), egui::vec2(rect.width() - 26.0, 26.0)),
        "Actions",
        c.fg,
        15.0,
        true,
    );
    let mut action = None;
    for (i, (label, name, icon, enabled)) in [
        ("Copy output", "copy", Icon::Copy, row.done && !row.alt_seen),
        ("Pipe output…", "pipe", Icon::Pipe, false),
        ("Re-run", "rerun", Icon::Rerun, row.done && !row.cmd.is_empty()),
        ("View in scrollback", "view", Icon::View, true),
    ]
    .into_iter()
    .enumerate()
    {
        let button = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 7.0, divider + 42.0 + i as f32 * 37.0),
            egui::vec2(rect.width() - 14.0, 34.0),
        );
        let response = ui.interact(
            button,
            ui.scope_id().with(("history_action", name)),
            if enabled { egui::Sense::click() } else { egui::Sense::hover() },
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
        let highlight = enabled && (response.hovered() || response.has_focus());
        if highlight {
            ui.painter().rect(
                button,
                5.0,
                c.bg.lerp_to_gamma(c.accent, 0.14),
                egui::Stroke::new(1.0, c.accent),
                egui::StrokeKind::Inside,
            );
        }
        let color = if !enabled {
            c.bg.lerp_to_gamma(c.muted, 0.65)
        } else if highlight {
            c.accent
        } else {
            c.fg
        };
        draw_icon(ui.painter(), icon, egui::pos2(button.left() + 18.0, button.center().y), color);
        text(ui, button.with_min_x(button.left() + 39.0), label, color, 14.0, false);
        if response.clicked() && enabled {
            action = Some(name);
        }
        if !enabled {
            response.on_hover_text(if name == "pipe" {
                "Piping output is not available yet"
            } else if !row.done {
                "Command is still running"
            } else if name == "rerun" {
                "No command line to re-run"
            } else {
                "No captured output available"
            });
        } else {
            response.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
    }
    action
}

/// The rename box: a small frameless window with one text field. Enter
/// commits, anything that drops the focus (Escape, a click elsewhere)
/// cancels. Returns whatever `UiState::rename_out` received this frame.
pub fn rename_box(ui: &egui::Ui, st: &mut UiState) {
    if !st.rename_open {
        st.rename_out = None;
        return;
    }
    let ctx = ui.ctx();
    let pos = egui::pos2(st.rename_pos[0], st.rename_pos[1]);
    let seed = st.rename_seed;
    let mut out: Option<Option<String>> = None;
    egui::Window::new("rename_tab")
        .id(egui::Id::new("rename_tab_box"))
        .title_bar(false)
        .movable(false)
        .resizable(false)
        .fixed_pos(pos)
        .constrain(false)
        .show(&ctx, |ui| {
            // Deliberately no lock_focus: losing the focus *is* the cancel
            // gesture (Escape, or a click anywhere else in the window).
            let te = egui::TextEdit::singleline(&mut st.rename_draft).desired_width(280.0);
            let resp = ui.add(te);
            if seed {
                resp.request_focus();
                st.rename_seed = false;
                // egui 0.37 dropped select_all_on, so pre-select through the
                // edit's stored cursor state: the whole draft reads as
                // selected text, and the first typed key replaces it.
                let id = resp.id;
                let len = st.rename_draft.chars().count();
                let mut state =
                    egui::text_edit::TextEditState::load(&ctx, id).unwrap_or_default();
                state.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                    egui::epaint::text::cursor::CCursor::new(0),
                    egui::epaint::text::cursor::CCursor::new(len),
                )));
                state.store(&ctx, id);
            }
            if resp.lost_focus() {
                // egui drops the edit's focus on Enter and on Escape alike;
                // only Enter commits. A click on another widget lands here.
                out = Some(if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    Some(st.rename_draft.clone())
                } else {
                    None
                });
            } else if ctx.input(|i| {
                // A press on empty background (the terminal, mostly) does
                // not steal egui focus, so treat it as cancel by hand. The
                // app also surrenders egui focus on its own mouse presses;
                // whichever fires first settles it.
                i.pointer.any_pressed()
                    && i
                        .pointer
                        .interact_pos()
                        .is_some_and(|p| !resp.rect.expand(8.0).contains(p))
            }) {
                out = Some(None);
            }
        });
    st.rename_out = out;
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
            let mut warm_st = UiState::default();
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
            config_changed: false,
            save_requested: false,
            revert_requested: false,
            ui_state: UiState::default(),
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
#[derive(Clone, Copy, Default)]
pub enum SwitcherStatus {
    #[default]
    Unknown,
    Running,
    Passed,
    Failed,
    Completed,
}

pub struct SwitcherCard {
    pub status: SwitcherStatus,
    pub title: String,
    /// The tab that is actually active (marked, like the strip does).
    pub active: bool,
    /// The entangled-tab marker the strip shows.
    pub marker: bool,
    /// Working directory shown in the card tooltip.
    pub cwd: Option<String>,
    /// A readback of the tab's live frame: the texture plus its pixel size.
    /// The size follows the terminal area's own aspect, so the image needs
    /// no letterboxing or cropping.
    pub thumb: Option<(egui::TextureHandle, (u32, u32))>,
}

/// Columns shared by preview sizing, drawing, and arrow-key navigation.
pub fn switcher_columns(screen_width: f32, count: usize) -> usize {
    ((((screen_width * 0.84).min(1400.0) - 36.0 + CARD_GAP) / 220.0).floor().max(1.0) as usize).min(4).min(count.max(1))
}

const CARD_GAP: f32 = 14.0;
const CARD_PAD: f32 = 8.0;
const CARD_HEADER: f32 = 40.0;

/// Preview dimensions fit the responsive grid and retain the terminal aspect.
pub fn thumbnail_size(screen_width: f32, aspect: f32, max_tw: u32, max_th: u32) -> (f32, f32) {
    if aspect <= 0.0 {
        return (0.0, 0.0);
    }
    let cols = switcher_columns(screen_width, usize::MAX) as f32;
    let panel_w = (screen_width * 0.84).min(1400.0);
    let box_w = ((panel_w - 36.0 - CARD_GAP * (cols - 1.0)) / cols - CARD_PAD * 2.0).min(max_tw as f32).max(48.0);
    let box_h = (max_th as f32).max(1.0);
    if box_w / box_h > aspect {
        (box_h * aspect, box_h)
    } else {
        (box_w, box_w / aspect)
    }
}

/// Release egui keyboard focus. A TextEdit that vanishes with its panel or
/// popup does NOT release focus by itself, and while egui holds focus,
/// egui-winit reports every key event as consumed -- the app then drops all
/// keystrokes and the terminal silently stops receiving input ("letters
/// vanish"). Call this whenever a UI region that could own the keyboard
/// steps aside, or when the app claims a click for itself.
pub fn release_focus(ctx: &egui::Context) {
    if let Some(id) = ctx.memory(|m| m.focused()) {
        ctx.memory_mut(|m| m.surrender_focus(id));
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

/// egui's look, built from the terminal's own colours: an egui panel that
/// ignores the theme the user configured for the terminal would be a second,
/// uglier app sharing the window. Everything derived (weak text, panel mix,
/// borders) is a mix between the configured background and foreground, so
/// light *and* dark palettes both get a coherent UI.
/// The switcher backdrop: the theme's background pressed towards dark,
/// not flat black. Over a light theme a black dim reads as smoke; over a
/// dark one the difference is subtle - either way the overlay now belongs
/// to the palette the user chose.
pub fn dim_fill(cfg: &crate::config::Config, bg: [f32; 3], alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        (bg[0] * 0.30 * 255.0) as u8,
        (bg[1] * 0.30 * 255.0) as u8,
        (bg[2] * 0.30 * 255.0) as u8,
        alpha,
    )
}

/// The Alt-held switcher keeps its live previews inside a centered surface.
/// The grid scrolls on small windows and follows keyboard selection.
pub fn switcher_screen(
    ui: &mut egui::Ui,
    open: bool,
    selected: usize,
    dim_fill: egui::Color32,
    accent: egui::Color32,
    cards: &[SwitcherCard],
    preview: egui::Vec2,
    pick: &mut Option<usize>,
) {
    if !open || cards.is_empty() {
        return;
    }
    let mut colors =
        TermStyle::new(ui, egui::Color32::from_rgb(104, 239, 145), egui::Color32::from_rgb(245, 115, 126));
    colors.accent = accent;
    let screen = ui.max_rect();
    let cols = switcher_columns(screen.width(), cards.len());
    let card_size = egui::vec2((preview.x + CARD_PAD * 2.0).max(204.0), preview.y + CARD_HEADER + CARD_PAD);
    let grid_w = card_size.x * cols as f32 + CARD_GAP * (cols - 1) as f32;
    let rows = cards.len().div_ceil(cols);
    let grid_h = card_size.y * rows as f32 + CARD_GAP * (rows - 1) as f32;
    let width = (grid_w + 36.0).max(440.0).min(screen.width() - 24.0);
    let viewport_h = grid_h.min((screen.height() - 180.0).max(80.0));
    let height = viewport_h + 134.0;
    let origin = egui::pos2(screen.center().x - width * 0.5, screen.center().y - height * 0.5);
    let selection_id = egui::Id::unique("switcher_last_selection");
    let view = (selected, cols, [screen.width().to_bits(), screen.height().to_bits(), preview.x.to_bits(), preview.y.to_bits()]);
    let previous = ui.ctx().data_mut(|d| {
        let previous = d.get_temp::<(usize, usize, [u32; 4])>(selection_id);
        d.insert_temp(selection_id, view);
        previous
    });
    let follow_selection = previous != Some(view);
    egui::CentralPanel::no_frame().frame(egui::Frame::NONE.fill(dim_fill)).show(ui, |ui| {
        ui.interact(ui.max_rect(), egui::Id::unique("switcher_backdrop"), egui::Sense::click());
    });
    egui::Area::new(egui::Id::unique("switcher_surface"))
        .order(egui::Order::Foreground)
        .fixed_pos(origin)
        .movable(false)
        .fade_in(false)
        .show(ui.ctx(), |ui| {
            ui.set_width(width);
            egui::Frame::new()
                .fill(colors.bg)
                .stroke(egui::Stroke::new(1.0, colors.border))
                .corner_radius(18.0)
                .inner_margin(18.0)
                .show(ui, |ui| {
                    ui.set_width(width - 36.0);
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                    let (header, _) =
                        ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
                    draw_icon(
                        ui.painter(),
                        Icon::Layers,
                        egui::pos2(header.left() + 15.0, header.center().y),
                        colors.fg,
                    );
                    let title =
                        ui.painter().layout_no_wrap("Tab Switcher".into(), egui::FontId::proportional(24.0), colors.fg);
                    let title_x = header.left() + 42.0;
                    ui.painter().galley(
                        egui::pos2(title_x, header.center().y - title.size().y * 0.5),
                        title.clone(),
                        colors.fg,
                    );
                    let divider = title_x + title.size().x + 22.0;
                    ui.painter().vline(
                        divider,
                        header.center().y - 11.0..=header.center().y + 11.0,
                        egui::Stroke::new(1.0, colors.border),
                    );
                    text(
                        ui,
                        header.with_min_x(divider + 20.0),
                        &format!("{} open terminal{}", cards.len(), if cards.len() == 1 { "" } else { "s" }),
                        colors.muted,
                        14.0,
                        false,
                    );
                    ui.add_space(12.0);
                    egui::ScrollArea::vertical()
                        .id_salt("switcher_grid")
                        .animated(false)
                        .auto_shrink([false, false])
                        .max_height(viewport_h)
                        .show(ui, |ui| {
                            let inset = ((ui.available_width() - grid_w) * 0.5).max(0.0);
                            for row in 0..rows {
                                let (row_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), card_size.y),
                                    egui::Sense::hover(),
                                );
                                for col in 0..cols {
                                    let i = row * cols + col;
                                    if i >= cards.len() {
                                        break;
                                    }
                                    let rect = egui::Rect::from_min_size(
                                        row_rect.min + egui::vec2(inset + col as f32 * (card_size.x + CARD_GAP), 0.0),
                                        card_size,
                                    );
                                    let response = switcher_card(ui, rect, i, &cards[i], i == selected, &colors);
                                    if response.clicked() {
                                        *pick = Some(i);
                                    }
                                    if i == selected && follow_selection {
                                        ui.scroll_to_rect(rect, None);
                                    }
                                }
                                if row + 1 < rows {
                                    ui.add_space(CARD_GAP);
                                }
                            }
                        });
                    ui.add_space(18.0);
                    if ui.available_width() < 650.0 {
                        let (help, _) =
                            ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
                        text(
                            ui,
                            help,
                            "Tab / ⇧Tab: move · release Alt: switch · Esc: cancel",
                            colors.muted,
                            11.0,
                            false,
                        );
                    } else {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 7.0;
                            let full = ui.available_width();
                            // Keep the help centered; its labels describe the actual Alt-held controls.
                            ui.add_space(((full - 650.0) * 0.5).max(0.0));
                            for (key, label) in
                                [("Tab / Shift-Tab", "move"), ("Enter / release Alt", "switch"), ("Esc", "cancel")]
                            {
                                egui::Frame::new()
                                    .fill(colors.card)
                                    .stroke(egui::Stroke::new(1.0, colors.border))
                                    .corner_radius(5.0)
                                    .inner_margin(egui::vec2(7.0, 4.0))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(key).monospace().size(12.0).color(colors.fg));
                                    });
                                ui.label(egui::RichText::new(label).size(12.0).color(colors.muted));
                                ui.add_space(6.0);
                            }
                        });
                    }
                });
        });
}

fn switcher_card(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    index: usize,
    card: &SwitcherCard,
    selected: bool,
    c: &TermStyle,
) -> egui::Response {
    let response = ui.interact(rect, egui::Id::unique("switcher_card").with(index), egui::Sense::click());
    let p = ui.painter();
    c.card_surface(p, rect, selected, response.hovered());
    let badge = egui::Rect::from_min_size(rect.min + egui::vec2(14.0, 9.0), egui::vec2(28.0, 24.0));
    c.number_badge(p, badge, index + 1, selected, 16.0);
    let (status, color) = match card.status {
        SwitcherStatus::Running => ("running", c.ok),
        SwitcherStatus::Passed => ("passed", c.ok),
        SwitcherStatus::Failed => ("failed", c.err),
        SwitcherStatus::Completed => ("done", c.muted),
        SwitcherStatus::Unknown => (if card.active { "active" } else { "terminal" }, c.muted),
    };
    let show_status = rect.width() >= 270.0;
    let status_width = if show_status { 78.0 } else { 20.0 };
    let dot = egui::pos2(rect.right() - status_width, badge.center().y);
    p.circle_filled(dot, 3.5, color);
    if show_status {
        text(
            ui,
            egui::Rect::from_min_max(dot + egui::vec2(9.0, -12.0), egui::pos2(rect.right() - 8.0, dot.y + 12.0)),
            status,
            color,
            12.0,
            false,
        );
    }
    let title = if card.marker { format!("* {}", card.title) } else { card.title.clone() };
    text(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(badge.right() + 12.0, badge.top()),
            egui::pos2(dot.x - 9.0, badge.bottom()),
        ),
        &title,
        c.fg,
        16.0,
        true,
    );
    let well = egui::Rect::from_min_max(
        rect.min + egui::vec2(CARD_PAD, CARD_HEADER),
        rect.max - egui::vec2(CARD_PAD, CARD_PAD),
    );
    p.rect(well, 7.0, c.bg, egui::Stroke::NONE, egui::StrokeKind::Inside);
    if let Some((texture, (w, h))) = &card.thumb {
        let native = egui::vec2(*w as f32, *h as f32);
        let scale = (well.width() / native.x).min(well.height() / native.y).min(1.0);
        let size = native * scale;
        let image = egui::Rect::from_center_size(well.center(), size);
        egui::Image::new(egui::load::SizedTexture::from(texture))
            .fit_to_exact_size(size)
            .corner_radius(7.0)
            .paint_at(ui, image);
    } else {
        text(ui, well.shrink(10.0), "Preparing preview…", c.muted, 12.0, false);
    }
    p.rect_stroke(well, 7.0, egui::Stroke::new(1.0, c.border), egui::StrokeKind::Inside);
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!(
        "{}\n{}\n{status}",
        card.title,
        card.cwd.as_deref().unwrap_or("")
    ))
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

/// Settings shares the history sidebar's palette and outlined controls.
/// Fonts and colours preview live; Save persists them and Cancel reloads
/// the saved configuration. The action row stays outside the scroll area.
pub fn config_window(
    ui: &mut egui::Ui,
    open: &mut bool,
    cfg: &mut crate::config::Config,
    st: &mut UiState,
    choices: &[crate::fonts::FontChoice],
) -> ConfigEvents {
    let mut ev = ConfigEvents { apply: false, save: false, revert: false };
    if !*open { return ev; }
    let colors = TermStyle::new(ui, rgb_c32(cfg.col[2]), rgb_c32(cfg.col[1]));
    let top = st.panel_top;
    let height = (ui.max_rect().bottom() - top).max(180.0);
    egui::Window::new("Settings")
        .id(egui::Id::unique("config_panel"))
        .title_bar(false)
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(0.0, top))
        .resizable([true, false])
        .default_width(448.0)
        .min_width(412.0)
        .max_width(640.0)
        .min_height(height).max_height(height).default_height(height)
        .fade_in(false)
        .fade_out(false)
        .frame(egui::Frame::new().fill(colors.bg).inner_margin(18.0))
        .show(ui.ctx(), |ui| {
            colors.apply_controls(ui);
            let edge = ui.max_rect().left() - 18.0;
            ui.painter().vline(edge, top..=ui.max_rect().bottom() + 18.0, egui::Stroke::new(1.0, colors.border));
            let (header, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::hover());
            draw_icon(ui.painter(), Icon::Settings, egui::pos2(header.left() + 13.0, header.center().y), colors.fg);
            text(ui, header.with_min_x(header.left() + 36.0).with_max_x(header.right() - 40.0), "Settings", colors.fg, 16.0, true);
            let close = egui::Rect::from_center_size(egui::pos2(header.right() - 12.0, header.center().y), egui::vec2(30.0, 30.0));
            if icon_button(ui, close, Icon::Dismiss, "Close settings", &colors).clicked() { *open = false; }
            ui.label(egui::RichText::new("Make this terminal your own.").small().color(colors.muted));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                for (page, label) in [
                    (SettingsPage::Appearance, "Appearance"),
                    (SettingsPage::Window, "Window"),
                    (SettingsPage::Tabs, "Tabs"),
                    (SettingsPage::Terminal, "Terminal"),
                ] {
                    let selected = st.settings_page == page;
                    let response = ui.add(egui::Button::new(egui::RichText::new(label).color(if selected { colors.accent } else { colors.muted }))
                        .corner_radius(17.0).min_size(egui::vec2(0.0, 34.0))
                        .fill(if selected { colors.bg.lerp_to_gamma(colors.accent, 0.16) } else { colors.card })
                        .stroke(egui::Stroke::new(1.0, if selected { colors.accent } else { colors.border })));
                    if response.clicked() {
                        st.settings_page = page;
                        st.font_popup = None;
                    }
                }
            });
            ui.add_space(12.0);
            egui::ScrollArea::vertical()
                .id_salt(("settings_body", st.settings_page as u8))
                .auto_shrink([false, false])
                .max_height((ui.available_height() - 86.0).max(40.0))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 12.0;
                    match st.settings_page {
                        SettingsPage::Appearance => {
                            section(ui, "Fonts", &colors, |ui| {
                                form_grid("fonts").show(ui, |ui| {
                                    font_row(ui, "Terminal font", &mut cfg.term_font, FontWhich::Term, st, choices, &mut ev);
                                    font_row(ui, "Tab font", &mut cfg.tablist_font, FontWhich::Tablist, st, choices, &mut ev);
                                });
                            });
                            section(ui, "Colors", &colors, |ui| {
                                if !st.theme_fetched {
                                    st.theme_choice = crate::theme::available();
                                    st.theme_fetched = true;
                                }
                                form_grid("colors-terminal").show(ui, |ui| {
                                    row(ui, "Theme", |ui| {
                                        let written = if cfg.theme.is_empty() { "default".to_string() } else { cfg.theme.clone() };
                                        let mut names = st.theme_choice.clone();
                                        if !names.contains(&written) { names.push(written.clone()); }
                                        let mut chosen = written.clone();
                                        egui::ComboBox::from_id_salt("theme_preset").popup_style((**ui.style()).clone().into())
                                            .width(170.0).selected_text(&written)
                                            .show_ui(ui, |ui| {
                                                for name in &names { ui.selectable_value(&mut chosen, name.clone(), name); }
                                            });
                                        if chosen != written {
                                            cfg.theme = chosen.clone();
                                            if let Err(e) = crate::theme::apply_name(cfg, &chosen) { log::warn!("theme {chosen}: {e}"); }
                                            ev.apply = true;
                                        }
                                    });
                                    color_row(ui, "Background", &mut cfg.background_color, &mut ev);
                                    color_row(ui, "Foreground", &mut cfg.foreground_color, &mut ev);
                                    color_row(ui, "Selection", &mut cfg.selection_color, &mut ev);
                                    color_row(ui, "Cursor", &mut cfg.cursor_color, &mut ev);
                                    color_row(ui, "Accent", &mut cfg.accent_color, &mut ev);
                                    color_row(ui, "Background tint", &mut cfg.bg_tint_color, &mut ev);
                                });
                            });
                        }
                        SettingsPage::Window => {
                            section(ui, "Window", &colors, |ui| {
                                form_grid("window").show(ui, |ui| {
                                    row(ui, "Transparency", |ui| {
                                        ev.apply |= ui.add(egui::Slider::new(&mut cfg.term_transparency, 0.0..=1.0)).changed();
                                    });
                                    row(ui, "Default width", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.default_width).suffix(" px")).changed();
                                    });
                                    row(ui, "Default height", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.default_height).suffix(" px")).changed();
                                    });
                                    checkbox_row(ui, "Start maximized", &mut cfg.default_maximize, &mut ev);
                                });
                            });
                        }
                        SettingsPage::Tabs => {
                            section(ui, "Tab strip", &colors, |ui| {
                                form_grid("tabs").show(ui, |ui| {
                                    row(ui, "Title length", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.tab_title_max_length)).changed();
                                    });
                                    row(ui, "Active title length", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.tab_title_short_current)).changed();
                                    });
                                    checkbox_row(ui, "Strip on top", &mut cfg.tablist_top, &mut ev);
                                    checkbox_row(ui, "Hide strip", &mut cfg.tablist_hide, &mut ev);
                                    checkbox_row(ui, "Multiline tabs", &mut cfg.multiline_tabs, &mut ev);
                                    checkbox_row(ui, "Hide single tab", &mut cfg.hide_single_tab, &mut ev);
                                    checkbox_row(ui, "Focus new tabs", &mut cfg.focus_new_tabs, &mut ev);
                                    checkbox_row(ui, "Shorten from start", &mut cfg.tab_title_shorten_start, &mut ev);
                                });
                            });
                            section(ui, "Tab colors", &colors, |ui| {
                                form_grid("colors-tabs").show(ui, |ui| {
                                    color_row(ui, "Strip", &mut cfg.tablist_background, &mut ev);
                                    color_row(ui, "Tab", &mut cfg.tab_color_bg, &mut ev);
                                    color_row(ui, "Tab (active)", &mut cfg.tab_color_bg_selected, &mut ev);
                                    color_row(ui, "Tab text", &mut cfg.tab_color_fg, &mut ev);
                                    color_row(ui, "Tab text (active)", &mut cfg.tab_color_fg_selected, &mut ev);
                                    color_row(ui, "Add button", &mut cfg.add_button_color, &mut ev);
                                });
                            });
                            section(ui, "Tab group colors", &colors, |ui| {
                                ui.horizontal(|ui| {
                                    for (i, color) in cfg.col.iter_mut().enumerate() {
                                        ui.vertical(|ui| {
                                            ui.label(egui::RichText::new(format!("{}", i + 1)).small().color(colors.muted));
                                            ev.apply |= color_cell(ui, color);
                                        });
                                    }
                                });
                            });
                            section(ui, "Tab switcher", &colors, |ui| {
                                form_grid("tab_switcher").show(ui, |ui| {
                                    checkbox_row(ui, "Open on Alt hold", &mut cfg.tab_overview_on_alt, &mut ev);
                                    row(ui, "Backdrop dim", |ui| {
                                        ev.apply |= ui.add(egui::Slider::new(&mut cfg.tab_overview_opacity, 0.0..=1.0)).changed();
                                    });
                                    row(ui, "Delay (ms)", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.tab_overview_delay_ms)).changed();
                                    });
                                    row(ui, "Thumb width", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.tab_overview_thumb_width)).changed();
                                    });
                                    row(ui, "Thumb height", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.tab_overview_thumb_height)).changed();
                                    });
                                });
                            });
                        }
                        SettingsPage::Terminal => {
                            section(ui, "Scrollback", &colors, |ui| {
                                form_grid("terminal").show(ui, |ui| {
                                    row(ui, "Scrollback lines", |ui| {
                                        ev.apply |= ui.add(egui::DragValue::new(&mut cfg.scrollback_lines).range(0..=1_000_000)).changed();
                                    });
                                });
                            });
                        }
                    }
                });
            ui.add_space(12.0);
            ui.separator();
            ui.label(egui::RichText::new("Changes preview instantly.").small().color(colors.muted));
            let (save, cancel) = actions(ui, ui.available_width(), "Save", "Cancel");
            if save || cancel {
                ev.save = save;
                ev.revert = cancel;
                st.theme_fetched = false;
                *open = false;
            }
        });
    if !*open { st.font_popup = None; }
    ev
}

fn row(ui: &mut egui::Ui, label: &str, add_control: impl FnOnce(&mut egui::Ui)) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(158.0, 32.0), egui::Sense::hover());
    text(ui, rect, label, ui.visuals().text_color(), 13.0, false);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, ui.is_enabled(), label));
    response.on_hover_text(label);
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
        let r = ui.add_sized([174.0, 30.0], egui::Button::new(shown).truncate())
            .on_hover_text(cfg_value.as_str());
        // One click does the whole job: closed opens, open (this row)
        // closes, the *other* row's popup is swapped for this one. The
        // toggle closes on PRESS, not on the click: a release-triggered
        // close would run before this frame's `clicked()` and the same
        // click would immediately re-open the popup (visible as "nothing
        // happens"). The suppress flag swallows the matching release.
        let same = st.font_popup.as_ref().is_some_and(|p| p.which == which);
        let pressed = ui.input(|i| i.pointer.any_pressed());
        if r.hovered() && pressed && same {
            st.font_popup = None;
            st.font_suppress = true;
        }
        if r.clicked() && !st.font_suppress && !same {
            st.font_popup = Some(seed_popup(which, cfg_value, choices));
        }
        if ui.input(|i| i.pointer.any_released()) {
            st.font_suppress = false;
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
                .style(crate::ui_style::picker_style(ui))
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
                                ui.add_space(2.0);
                                let (ok, canc) = actions(ui, 250.0, "OK", "Cancel");
                                if ok || dbl {
                                    commit = compose_value(
                                        choices,
                                        &popup.family,
                                        popup.style,
                                        popup.size_pt,
                                    );
                                    close = true;
                                }
                                if canc {
                                    close = true;
                                }
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
        ui.horizontal(|ui| {
            if color_cell(ui, c) { ev.apply = true; }
            ui.label(egui::RichText::new(format!("#{:02X}{:02X}{:02X}", c.0, c.1, c.2))
                .monospace().color(ui.visuals().weak_text_color()));
        });
    });
}

/// The color button alone (the control side of an aligned row).
fn color_cell(ui: &mut egui::Ui, c: &mut crate::config::Rgb) -> bool {
    let mut color = rgb_c32(*c);
    let response = ui.add_sized([30.0, 28.0], egui::Button::new("").fill(color)
        .stroke(ui.visuals().widgets.inactive.bg_stroke).corner_radius(5.0))
        .on_hover_text(format!("Edit #{:02X}{:02X}{:02X}", c.0, c.1, c.2));
    let mut changed = false;
    egui::Popup::menu(&response)
        .style((**ui.style()).clone())
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.spacing_mut().slider_width = 275.0;
            changed = egui::color_picker::color_picker_color32(ui, &mut color, egui::color_picker::Alpha::Opaque);
        });
    if changed { *c = crate::config::Rgb(color.r(), color.g(), color.b()); }
    changed
}

fn checkbox_row(ui: &mut egui::Ui, label: &str, v: &mut bool, ev: &mut ConfigEvents) {
    row(ui, label, |ui| ev.apply |= crate::ui_style::toggle(ui, label, v));
}




#[cfg(test)]
mod regression {
    use super::*;

    /// One layout pass of the config panel at a given screen size.
    fn pass(ctx: &Context, w: f32, h: f32, open: bool) -> egui::FullOutput {
        let mut st = UiState::default();
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

    #[test]
    fn settings_pages_keep_save_and_cancel_visible_after_resize() {
        let ctx = Context::default();
        let mut st = UiState { panel_top: 24.0, ..Default::default() };
        let mut cfg = crate::config::Config::default();
        let mut open = true;
        for (w, h) in [(1280.0, 850.0), (900.0, 540.0), (640.0, 420.0)] {
            for page in [SettingsPage::Appearance, SettingsPage::Window, SettingsPage::Tabs, SettingsPage::Terminal] {
                st.settings_page = page;
                for _ in 0..3 {
                    let mut full = ctx.run_ui(egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))),
                        ..Default::default()
                    }, |ui| { config_window(ui, &mut open, &mut cfg, &mut st, &[]); });
                    full.textures_delta.clear();
                    for label in ["Save", "Cancel"] {
                        let text = full.shapes.iter().find_map(|s| match &s.shape {
                            egui::Shape::Text(t) if t.galley.text() == label => Some((s.clip_rect, t)),
                            _ => None,
                        });
                        // The first sizing pass of a new window doesn't paint.
                        if let Some((clip, text)) = text {
                            let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                            assert!(rect.min.y >= 24.0 && rect.max.y < h, "{label} is outside the window: {rect:?}");
                            assert!(clip.contains_rect(rect), "{label} is clipped: {rect:?} by {clip:?}");
                        }
                    }
                }
                let mut full = ctx.run_ui(egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))),
                    ..Default::default()
                }, |ui| { config_window(ui, &mut open, &mut cfg, &mut st, &[]); });
                for label in ["Save", "Cancel"] {
                    assert!(full.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text() == label)), "{label} missing");
                }
                full.textures_delta.clear();
            }
        }
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

        let mut st = UiState::default();
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

    #[test]
    fn switcher_keeps_selection_and_previews_inside_the_surface() {
        let ctx = Context::default();
        for (w, h) in [(1920.0, 1080.0), (560.0, 480.0)] {
            let (tw, th) = thumbnail_size(w, 1.78, 300, 225);
            let (tw, th) = (tw.round() as u32, th.round() as u32);
            let tex = ctx.load_texture("preview", egui::ColorImage::new(
                [tw as usize, th as usize], vec![egui::Color32::BLACK; (tw * th) as usize]), egui::TextureOptions::LINEAR);
            let cards: Vec<SwitcherCard> = (0..8).map(|i| SwitcherCard {
                title: format!("a much longer tab title number {i}"),
                status: SwitcherStatus::Unknown,
                active: i == 1,
                marker: false,
                cwd: Some("~/projects".into()),
                thumb: Some((tex.clone(), (tw, th))),
            }).collect();
            for selected in [7, 0, 7] {
                let mut output = None;
                for _ in 0..5 {
                    let mut full = ctx.run_ui(egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))),
                        ..Default::default()
                    }, |ui| switcher_screen(ui, true, selected, egui::Color32::from_black_alpha(150), egui::Color32::CYAN, &cards, egui::vec2(tw as f32, th as f32), &mut None));
                    full.textures_delta.clear();
                    output = Some(full);
                }
                let full = output.unwrap();
                let (clip, selection) = full.shapes.iter().find_map(|cs| match &cs.shape {
                    egui::Shape::Rect(r) if r.stroke.width == 2.5 => Some((cs.clip_rect, r.rect)),
                    _ => None,
                }).expect("selected card missing");
                assert!(clip.expand(1.0).contains_rect(selection), "selected card is clipped: {selection:?} by {clip:?}");
                assert!(selection.min.x >= 0.0 && selection.max.x <= w && selection.min.y >= 0.0 && selection.max.y <= h);
                let images: Vec<_> = full.shapes.iter().filter_map(|cs| match &cs.shape {
                    egui::Shape::Rect(r) if r.fill_texture_id() == tex.id() => Some(r.rect),
                    _ => None,
                }).collect();
                assert!(!images.is_empty(), "live previews missing");
                for image in images {
                    assert!((image.width() - tw as f32).abs() < 1.0 && (image.height() - th as f32).abs() < 1.0, "preview was distorted");
                    if image.intersects(selection) {
                        assert!(selection.contains_rect(image), "preview outside card");
                        assert!(image.top() >= selection.top() + 32.0, "preview overlaps header");
                    }
                }
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
        let mut st = UiState::default();
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
        let mut st = UiState::default();
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
    /// The egui rename box: seeded draft arrives selected (first key
    /// replaces it), Enter commits, and any other focus loss -- Escape or a
    /// click away -- cancels. The hand-drawn prompt had this logic in the
    /// app's key handler; egui owns it now.
    #[test]
    fn rename_box_selects_seeds_commits_and_cancels() {
        let ctx = Context::default();
        let mut st = UiState::default();
        let mut step = |st: &mut UiState, events: Vec<egui::Event>| {
            let mut full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_max(
                        egui::pos2(0.0, 0.0),
                        egui::pos2(900.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| rename_box(ui, st),
            );
            full.textures_delta.clear();
        };
        let key = |k: egui::Key| egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let click_far = egui::Event::PointerButton {
            pos: egui::pos2(800.0, 500.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        };

        st.rename_open = true;
        st.rename_seed = true;
        st.rename_draft = "worktree".to_string();
        step(&mut st, vec![]);
        // Focused, and the whole draft reads as selected text.
        let focused = ctx.memory(|m| m.focused()).expect("rename entry took focus");
        let sel = egui::text_edit::TextEditState::load(&ctx, focused)
            .and_then(|s| s.cursor.char_range())
            .expect("cursor state");
        assert_ne!(sel.primary, sel.secondary, "seeded draft is not selected");
        // The seed is a one-frame gesture.
        assert!(!st.rename_seed);

        // Typing over a selection replaces it -- the GTK behaviour the
        // hand-drawn draft used to emulate.
        step(
            &mut st,
            vec![egui::Event::Text("w".into())],
        );
        assert_eq!(st.rename_draft, "w", "typing did not replace the selection");

        // Enter commits exactly what is on screen.
        step(&mut st, vec![key(egui::Key::Enter)]);
        assert_eq!(
            st.rename_out.as_ref().map(Option::as_deref),
            Some(Some("w")),
            "Enter did not commit"
        );

        // A click elsewhere cancels: nothing is renamed.
        let mut st2 = UiState::default();
        st2.rename_open = true;
        st2.rename_seed = true;
        st2.rename_draft = "keep".to_string();
        step(&mut st2, vec![]);
        assert!(ctx.memory(|m| m.focused()).is_some());
        step(&mut st2, vec![click_far]);
        assert_eq!(
            st2.rename_out.as_ref().map(Option::as_deref),
            Some(None),
            "clicking away did not cancel"
        );
    }

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
        let mut st = UiState::default();
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
        let mut st = UiState::default();
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
        let mut st = UiState::default();
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

    fn row(seq: u64, cmd: &str, exit: Option<i32>) -> HistoryRow {
        HistoryRow {
            seq,
            cmd: cmd.to_string(),
            exit,
            done: true,
            alt_seen: false,
            lines: 1,
            time: "00:00:00".into(),
            duration: String::new(),
        }
    }

    #[test]
    fn history_filter_and_search_narrow_the_list() {
        let ok = row(1, "cargo test", Some(0));
        let fail = row(2, "cargo build", Some(2));
        let nos = row(3, "ls", None);
        // All keeps everything; Success keeps zero-exit and no-status;
        // Error keeps only nonzero.
        assert!(history_visible(&ok, HistoryFilter::All, ""));
        assert!(history_visible(&ok, HistoryFilter::Success, ""));
        assert!(!history_visible(&fail, HistoryFilter::Success, ""));
        assert!(!history_visible(&ok, HistoryFilter::Error, ""));
        assert!(history_visible(&fail, HistoryFilter::Error, ""));
        assert!(history_visible(&nos, HistoryFilter::Success, ""), "no status is not an error");
        // Search is case-insensitive on the command line.
        assert!(history_visible(&ok, HistoryFilter::All, "CARGO"));
        assert!(!history_visible(&ok, HistoryFilter::All, "build"));
        assert!(history_visible(&ok, HistoryFilter::All, ""), "empty query keeps everything");
    }
    #[test]
    fn sidebars_claim_keyboard_only_for_active_editors() {
        for panel in [RightPanel::History, RightPanel::Settings] {
            let ctx = Context::default();
            let mut st = UiState { right_panel: panel, ..Default::default() };
            let id = egui::Id::unique("editor_or_button");
            let mut text = String::new();
            // Reuse the editor's id for a button to exercise stale TextEditState.
            for editor in [false, true, false] {
                ctx.memory_mut(|m| m.request_focus(id));
                let mut full = ctx.run_ui(egui::RawInput::default(), |ui| {
                    if editor {
                        ui.add(egui::TextEdit::singleline(&mut text).id(id));
                    } else {
                        ui.interact(ui.max_rect(), id, egui::Sense::click());
                    }
                });
                st.release_passive_focus(&ctx, &full.platform_output);
                full.textures_delta.clear();
                assert_eq!(st.owns_keyboard(&ctx), editor, "{panel:?}: editor={editor}");
            }
        }
    }

    #[test]
    fn history_only_claims_pointer_input_inside_the_sidebar() {
        let ctx = Context::default();
        let mut st = UiState { right_panel: RightPanel::History, ..Default::default() };
        for (point, claims) in [(egui::pos2(100.0, 200.0), false), (egui::pos2(1100.0, 200.0), true)] {
            for _ in 0..3 {
                let mut full = ctx.run_ui(egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0))),
                    events: vec![egui::Event::PointerMoved(point)],
                    ..Default::default()
                }, |ui| history_panel(ui, &mut st, egui::Color32::GREEN, egui::Color32::RED, 24.0));
                full.textures_delta.clear();
            }
            assert_eq!(ctx.egui_wants_pointer_input(), claims, "pointer at {point:?}");
            // A subsequent event can carry a new position before the next
            // UI pass has updated egui's pointer state.
            assert!(!panel_pointer_input(&ctx, egui::pos2(100.0, 200.0)));
            assert!(panel_pointer_input(&ctx, egui::pos2(1100.0, 200.0)));
        }
    }

}
