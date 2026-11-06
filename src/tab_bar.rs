//! Compact egui tab strip. Geometry is returned to the window controller so
//! native drag-to-reorder and cross-window drops keep using physical positions.
use crate::ui_style::{self, Icon};

pub(crate) struct TabFace {
    pub id: usize,
    pub title: String,
    pub tooltip: String,
    pub group: Option<egui::Color32>,
    pub entangled: bool,
    pub attention: f32,
}

pub(crate) struct Geometry {
    pub tabs: Vec<egui::Rect>,
    pub close: Vec<egui::Rect>,
    pub add: egui::Rect,
}

impl Default for Geometry {
    fn default() -> Self {
        Self { tabs: Vec::new(), close: Vec::new(), add: egui::Rect::NOTHING }
    }
}

pub(crate) fn show(
    root: &mut egui::Ui,
    height: f32,
    font: egui::FontId,
    tabs: &[TabFace],
    active: usize,
    clock: Option<&str>,
    cfg: &crate::config::Config,
) -> Geometry {
    let chrome = ui_style::TabStyle::new(root, cfg);
    let colors = chrome.base;
    let width = root.max_rect().width();
    let mut geometry = Geometry {
        tabs: vec![egui::Rect::NOTHING; tabs.len()],
        close: vec![egui::Rect::NOTHING; tabs.len()],
        ..Default::default()
    };
    egui::Area::new(egui::Id::unique("terminal_tab_bar"))
        .fixed_pos(egui::Pos2::ZERO)
        .order(egui::Order::Middle)
        .movable(false)
        .constrain(false)
        .fade_in(false)
        .show(root.ctx(), |ui| {
            ui.set_min_size(egui::vec2(width, height));
            let p = ui.painter();
            let bar = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
            p.rect_filled(bar, 0.0, colors.bg);
            p.hline(bar.x_range(), height - 0.5, egui::Stroke::new(1.0, colors.border));
            let brand_width = if width >= 700.0 { 108.0 } else { 30.0 };
            ui_style::icon(p, Icon::Terminal, egui::pos2(15.0, height * 0.5), colors.muted);
            if brand_width > 30.0 {
                ui_style::text(
                    ui,
                    egui::Rect::from_min_max(egui::pos2(30.0, 0.0), egui::pos2(104.0, height)),
                    "mypixels",
                    colors.fg,
                    13.0,
                    true,
                );
            }
            p.vline(brand_width - 1.0, 0.0..=height, egui::Stroke::new(1.0, colors.border));
            let clock_width = if clock.is_some() && width >= 500.0 { 52.0 } else { 0.0 };
            if clock_width > 0.0 {
                ui_style::text(
                    ui,
                    egui::Rect::from_min_max(egui::pos2(width - clock_width, 0.0), bar.max),
                    clock.unwrap(),
                    colors.muted,
                    11.0,
                    false,
                );
            }
            let gap = 4.0;
            let body_height = (height - 6.0).max(16.0);
            let widths: Vec<f32> = tabs
                .iter()
                .map(|tab| {
                    let title = ui.painter().layout_no_wrap(tab.title.clone(), font.clone(), colors.fg);
                    (title.size().x + 58.0 + if tab.entangled { 10.0 } else { 0.0 }).clamp(96.0, 236.0)
                })
                .collect();
            let content_width = widths.iter().sum::<f32>() + gap * tabs.len().saturating_sub(1) as f32;
            let viewport_width = content_width.min((width - brand_width - clock_width - 38.0).max(30.0));
            let viewport =
                egui::Rect::from_min_size(egui::pos2(brand_width + 4.0, 3.0), egui::vec2(viewport_width, body_height));
            let selection = (tabs.get(active).map(|t| t.id), width.to_bits(), content_width.to_bits());
            let follow = ui.ctx().data_mut(|d| {
                let id = egui::Id::unique("tab_bar_selection");
                let old = d.get_temp::<(Option<usize>, u32, u32)>(id);
                d.insert_temp(id, selection);
                old != Some(selection)
            });
            let mut host = ui.new_child(egui::UiBuilder::new().id_salt("tab_bar_viewport").max_rect(viewport));
            host.set_clip_rect(viewport);
            egui::ScrollArea::horizontal()
                .id_salt("tab_bar_scroll")
                .animated(false)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .auto_shrink([false, false])
                .max_width(viewport_width)
                .max_height(body_height)
                .show(&mut host, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(gap, 0.0);
                    ui.spacing_mut().interact_size.y = body_height;
                    ui.horizontal(|ui| {
                        for (index, tab) in tabs.iter().enumerate() {
                            // Pointer actions stay in the window controller. This egui
                            // widget provides layout, hover, clipping and tooltip behavior.
                            let (rect, response) =
                                ui.allocate_exact_size(egui::vec2(widths[index], body_height), egui::Sense::hover());
                            let selected = index == active;
                            chrome.tab(ui.painter(), rect, selected, response.hovered());
                            if tab.attention > 0.0 {
                                ui.painter().rect_filled(
                                    rect.shrink(1.0),
                                    5.0,
                                    colors.err.gamma_multiply(tab.attention * 0.35),
                                );
                            }
                            let badge = egui::Rect::from_center_size(
                                egui::pos2(rect.left() + 15.0, rect.center().y),
                                egui::vec2(19.0, (body_height - 6.0).max(14.0)),
                            );
                            let mut badge_style = colors;
                            badge_style.fg = chrome.index;
                            badge_style.number_badge(ui.painter(), badge, index + 1, selected, 11.0);
                            let close = egui::Rect::from_center_size(
                                egui::pos2(rect.right() - 13.0, rect.center().y),
                                egui::vec2(20.0, body_height),
                            );
                            let title = rect.with_min_x(badge.right() + 8.0).with_max_x(close.left() - 3.0);
                            if tab.entangled {
                                ui_style::text(
                                    ui,
                                    title.with_max_x(title.left() + 10.0),
                                    "*",
                                    chrome.entangle,
                                    13.0,
                                    true,
                                );
                            }
                            ui_style::text_with_font(
                                ui,
                                title.with_min_x(title.left() + if tab.entangled { 10.0 } else { 0.0 }),
                                &tab.title,
                                tab.group.unwrap_or(if selected {
                                    chrome.selected_foreground
                                } else {
                                    chrome.foreground
                                }),
                                font.clone(),
                            );
                            let close_hover = ui.rect_contains_pointer(close);
                            if selected || response.hovered() {
                                if close_hover {
                                    ui.painter().rect_filled(close.shrink(2.0), 4.0, colors.card);
                                }
                                ui_style::icon(
                                    ui.painter(),
                                    Icon::Close,
                                    close.center(),
                                    if close_hover { colors.accent } else { colors.muted },
                                );
                            }
                            geometry.tabs[index] = rect.intersect(viewport);
                            geometry.close[index] = close.intersect(viewport);
                            response.on_hover_text(&tab.tooltip).on_hover_cursor(egui::CursorIcon::PointingHand);
                            if selected && follow {
                                ui.scroll_to_rect(rect, None);
                            }
                        }
                    });
                });
            let add = egui::Rect::from_min_size(egui::pos2(viewport.right() + 4.0, 3.0), egui::vec2(28.0, body_height));
            let response = ui.interact(add, egui::Id::unique("tab_bar_add"), egui::Sense::hover());
            if response.hovered() {
                colors.surface(ui.painter(), add, 5.0, false, true);
            }
            ui_style::icon(
                ui.painter(),
                Icon::Plus,
                add.center(),
                if response.hovered() { colors.accent } else { chrome.add },
            );
            response.on_hover_text("New tab · Ctrl+Shift+T").on_hover_cursor(egui::CursorIcon::PointingHand);
            geometry.add = add;
        });
    geometry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_tabs_keep_the_active_tab_and_add_button_reachable() {
        for width in [360.0, 1280.0] {
            let ctx = egui::Context::default();
            let cfg = crate::config::Config::default();
            let tabs: Vec<_> = (0..40)
                .map(|id| TabFace {
                    id,
                    title: format!("long terminal name {id} 界"),
                    tooltip: String::new(),
                    group: None,
                    entangled: id == 3,
                    attention: 0.0,
                })
                .collect();
            for active in [0, 39, 1] {
                let mut geometry = Geometry::default();
                for _ in 0..5 {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 480.0))),
                            ..Default::default()
                        },
                        |ui| {
                            geometry =
                                show(ui, 28.0, egui::FontId::monospace(14.0), &tabs, active, Some("12:34"), &cfg);
                        },
                    );
                    output.textures_delta.clear();
                }
                let selected = geometry.tabs[active];
                assert!(selected.width() >= 90.0, "active tab is clipped: {selected:?}");
                assert!(selected.left() >= 0.0 && selected.right() < width);
                assert!(geometry.close[active].width() >= 18.0, "close control is clipped");
                assert!(geometry.add.right() <= width && geometry.add.left() >= selected.right());
                assert!(geometry.add.top() >= 0.0 && geometry.add.bottom() <= 28.0);
            }
        }
    }
}
