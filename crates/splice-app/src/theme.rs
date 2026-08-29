//! Custom egui style: dark AND light (system-following), accent #5B8DEF, 8 px rounding.
//! Deliberately not stock egui gray — fills, strokes and widget states are all tuned here.

use egui::{
    Color32, CornerRadius, FontId, Margin, Shadow, Stroke, TextStyle, Theme, Visuals, vec2,
};

pub const ACCENT: Color32 = Color32::from_rgb(0x5B, 0x8D, 0xEF);
pub const ACCENT_STRONG: Color32 = Color32::from_rgb(0x3F, 0x76, 0xE0);
pub const OK: Color32 = Color32::from_rgb(0x46, 0xB8, 0x6E);
pub const WARN: Color32 = Color32::from_rgb(0xE0, 0xA8, 0x3E);
pub const ERR: Color32 = Color32::from_rgb(0xDD, 0x5B, 0x51);

pub fn card_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(0x22, 0x26, 0x2F)
    } else {
        Color32::WHITE
    }
}

pub fn display_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(0x30, 0x36, 0x43)
    } else {
        Color32::from_rgb(0xE9, 0xED, 0xF5)
    }
}

pub fn card_border(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(0x39, 0x3F, 0x4D)
    } else {
        Color32::from_rgb(0xD8, 0xDD, 0xE6)
    }
}

pub fn canvas_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(0x14, 0x16, 0x1B)
    } else {
        Color32::from_rgb(0xF2, 0xF4, 0xF7)
    }
}

pub fn panel_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(0x1A, 0x1D, 0x24)
    } else {
        Color32::from_rgb(0xFA, 0xFB, 0xFC)
    }
}

/// sRGB mix of `a` toward `b` by `t` in 0..=1.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
}

/// Pull a color toward neutral gray (for disabled machines).
pub fn desaturate(c: Color32, amount: f32) -> Color32 {
    let gray = ((c.r() as u16 + c.g() as u16 + c.b() as u16) / 3) as u8;
    mix(c, Color32::from_rgba_unmultiplied(gray, gray, gray, c.a()), amount)
}

/// Apply opacity (for offline ghosting at 40%).
pub fn ghost(c: Color32, opacity: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * opacity) as u8)
}

pub fn apply(ctx: &egui::Context) {
    ctx.style_mut_of(Theme::Dark, |style| {
        style.visuals = visuals(true);
        tune_spacing_and_text(&mut style.spacing, &mut style.text_styles);
    });
    ctx.style_mut_of(Theme::Light, |style| {
        style.visuals = visuals(false);
        tune_spacing_and_text(&mut style.spacing, &mut style.text_styles);
    });
}

fn tune_spacing_and_text(
    spacing: &mut egui::style::Spacing,
    text_styles: &mut std::collections::BTreeMap<TextStyle, FontId>,
) {
    spacing.item_spacing = vec2(10.0, 8.0);
    spacing.button_padding = vec2(12.0, 6.0);
    spacing.window_margin = Margin::same(14);
    spacing.menu_margin = Margin::same(8);
    spacing.indent = 20.0;
    spacing.slider_width = 180.0;
    spacing.slider_rail_height = 6.0;
    spacing.icon_spacing = 8.0;

    for (style, size) in [
        (TextStyle::Small, 11.5),
        (TextStyle::Body, 14.0),
        (TextStyle::Button, 13.5),
        (TextStyle::Heading, 21.0),
        (TextStyle::Monospace, 13.0),
    ] {
        text_styles.insert(style, FontId::proportional(size));
    }
}

fn visuals(dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.dark_mode = dark;
    v.panel_fill = panel_fill(dark);
    v.window_fill = panel_fill(dark);
    v.faint_bg_color = if dark {
        Color32::from_rgb(0x1F, 0x23, 0x2B)
    } else {
        Color32::from_rgb(0xEE, 0xF1, 0xF5)
    };
    v.extreme_bg_color = if dark {
        Color32::from_rgb(0x0E, 0x10, 0x14)
    } else {
        Color32::from_rgb(0xE2, 0xE6, 0xEC)
    };
    v.code_bg_color = v.faint_bg_color;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = ERR;
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: if dark {
            Color32::from_black_alpha(90)
        } else {
            Color32::from_black_alpha(28)
        },
    };
    v.window_stroke = Stroke::new(1.0, card_border(dark));
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    let (text, text_weak, border, fill, fill_hover, fill_active) = if dark {
        (
            Color32::from_rgb(0xD6, 0xDB, 0xE4),
            Color32::from_rgb(0x8B, 0x93, 0xA2),
            Color32::from_rgb(0x3A, 0x40, 0x4E),
            Color32::from_rgb(0x27, 0x2C, 0x37),
            Color32::from_rgb(0x30, 0x36, 0x44),
            Color32::from_rgb(0x38, 0x3F, 0x50),
        )
    } else {
        (
            Color32::from_rgb(0x22, 0x26, 0x2D),
            Color32::from_rgb(0x6E, 0x76, 0x83),
            Color32::from_rgb(0xD8, 0xDD, 0xE6),
            Color32::from_rgb(0xEC, 0xEF, 0xF4),
            Color32::from_rgb(0xE1, 0xE7, 0xF1),
            Color32::from_rgb(0xD3, 0xDC, 0xEC),
        )
    };

    let radius = CornerRadius::same(8);
    v.widgets.noninteractive.corner_radius = radius;
    v.widgets.noninteractive.bg_fill = v.panel_fill;
    v.widgets.noninteractive.weak_bg_fill = v.faint_bg_color;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);

    v.widgets.inactive.corner_radius = radius;
    v.widgets.inactive.bg_fill = fill;
    v.widgets.inactive.weak_bg_fill = fill;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, text);

    v.widgets.hovered.corner_radius = radius;
    v.widgets.hovered.bg_fill = fill_hover;
    v.widgets.hovered.weak_bg_fill = fill_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.7));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, text);
    v.widgets.hovered.expansion = 0.5;

    v.widgets.active.corner_radius = radius;
    v.widgets.active.bg_fill = fill_active;
    v.widgets.active.weak_bg_fill = fill_active;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_STRONG);
    v.widgets.active.fg_stroke = Stroke::new(1.2, text);
    v.widgets.active.expansion = 0.5;

    v.widgets.open.corner_radius = radius;
    v.widgets.open.bg_fill = fill_hover;
    v.widgets.open.weak_bg_fill = fill_hover;
    v.widgets.open.bg_stroke = Stroke::new(1.0, border);
    v.widgets.open.fg_stroke = Stroke::new(1.0, text);

    v.weak_text_color = Some(text_weak);
    v
}
