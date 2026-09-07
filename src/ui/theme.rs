//! Colours and spacing.
//!
//! A small dark card that sits on top of whatever wallpaper the user has, so it needs enough
//! contrast against both a bright and a dark desktop. Status colour is carried by both a hue and
//! a shape (filled versus hollow marker), never by hue alone.

use egui::Color32;

pub const CARD: Color32 = Color32::from_rgb(22, 24, 30);
pub const BORDER: Color32 = Color32::from_rgb(52, 58, 70);
pub const TEXT: Color32 = Color32::from_rgb(232, 236, 244);
pub const MUTED: Color32 = Color32::from_rgb(150, 158, 174);
pub const ACTIVE: Color32 = Color32::from_rgb(76, 194, 140);
pub const IDLE: Color32 = Color32::from_rgb(104, 114, 132);
pub const WARN: Color32 = Color32::from_rgb(230, 175, 88);
pub const DANGER: Color32 = Color32::from_rgb(226, 106, 106);
pub const BTN: Color32 = Color32::from_rgb(38, 42, 52);
pub const BTN_HOVER: Color32 = Color32::from_rgb(50, 56, 68);
pub const BTN_ON: Color32 = Color32::from_rgb(41, 92, 71);

pub fn apply(ctx: &egui::Context) {
    // egui 0.36 keeps a style per theme; the widget is dark in both, so set both and pin the
    // theme so a light-mode desktop does not hand us an unreadable card.
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    let v = &mut style.visuals;
    v.dark_mode = true;
    v.override_text_color = Some(TEXT);
    v.panel_fill = CARD;
    v.window_fill = CARD;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.inactive.bg_fill = BTN;
    v.widgets.hovered.weak_bg_fill = BTN_HOVER;
    v.widgets.hovered.bg_fill = BTN_HOVER;
    v.widgets.active.weak_bg_fill = BTN_HOVER;
    v.widgets.active.bg_fill = BTN_HOVER;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    let style = std::sync::Arc::new(style);
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}
