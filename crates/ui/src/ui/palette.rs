//! Runtime colour palette: the two user-selectable colours (main and accent),
//! the catalog of presets, and the derivation that keeps every combination
//! readable.
//!
//! The app used to keep ~55 compile-time colours. Screens now read a
//! [`Palette`] owned by `App`; this module is the only place that spells a
//! colour literal for UI surfaces.

use gpui_kit::component::{Edges, Theme, ThemeMode};
use gpui_kit::{Anchor, App, px, rgb, rgba, transparent_black};

/// The three colour families offered for both selectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Dark,
    Light,
    Pastel,
}

impl Family {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Dark => "黒系",
            Self::Light => "白系",
            Self::Pastel => "パステルカラー系",
        }
    }

    pub(crate) fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MainColor {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) family: Family,
    pub(crate) hue: f32,
    pub(crate) tint: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AccentColor {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) family: Family,
    pub(crate) hue: f32,
    pub(crate) saturation: f32,
    pub(crate) lightness: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    pub(crate) is_dark: bool,
    pub(crate) is_pastel: bool,

    pub(crate) bg: u32,
    pub(crate) glass: u32,
    pub(crate) panel: u32,
    pub(crate) panel_2: u32,
    pub(crate) panel_sidebar: u32,
    pub(crate) panel_header: u32,
    pub(crate) panel_card: u32,
    pub(crate) panel_inner: u32,
    pub(crate) panel_deep: u32,

    pub(crate) border: u32,
    pub(crate) border_strong: u32,
    pub(crate) line_soft: u32,
    pub(crate) highlight: u32,
    pub(crate) shadow: u32,
    pub(crate) scrim: u32,

    pub(crate) text: u32,
    pub(crate) muted: u32,
    pub(crate) muted_2: u32,

    pub(crate) accent: u32,
    pub(crate) accent_fill: u32,
    pub(crate) accent_soft: u32,
    pub(crate) accent_soft_hover: u32,
    pub(crate) accent_border: u32,
    pub(crate) on_accent: u32,

    pub(crate) green: u32,
    pub(crate) green_soft: u32,
    pub(crate) amber: u32,
    pub(crate) amber_soft: u32,
    pub(crate) red: u32,
    pub(crate) red_soft: u32,
    pub(crate) red_border: u32,
}

pub(crate) const MAIN_COLORS: [MainColor; 18] = [
    MainColor {
        id: "charcoal",
        label: "チャコール",
        family: Family::Dark,
        hue: 220.0,
        tint: 1.0,
    },
    MainColor {
        id: "midnight",
        label: "ミッドナイト",
        family: Family::Dark,
        hue: 228.0,
        tint: 1.15,
    },
    MainColor {
        id: "forest",
        label: "フォレスト",
        family: Family::Dark,
        hue: 155.0,
        tint: 0.85,
    },
    MainColor {
        id: "plum",
        label: "プラム",
        family: Family::Dark,
        hue: 285.0,
        tint: 0.90,
    },
    MainColor {
        id: "wine",
        label: "ワイン",
        family: Family::Dark,
        hue: 350.0,
        tint: 0.85,
    },
    MainColor {
        id: "sepia",
        label: "セピア",
        family: Family::Dark,
        hue: 35.0,
        tint: 0.80,
    },
    MainColor {
        id: "paper",
        label: "ペーパー",
        family: Family::Light,
        hue: 220.0,
        tint: 1.0,
    },
    MainColor {
        id: "ivory",
        label: "アイボリー",
        family: Family::Light,
        hue: 45.0,
        tint: 0.45,
    },
    MainColor {
        id: "mist",
        label: "ミスト",
        family: Family::Light,
        hue: 205.0,
        tint: 0.55,
    },
    MainColor {
        id: "sage",
        label: "セージ",
        family: Family::Light,
        hue: 145.0,
        tint: 0.40,
    },
    MainColor {
        id: "blush",
        label: "ブラッシュ",
        family: Family::Light,
        hue: 345.0,
        tint: 0.35,
    },
    MainColor {
        id: "cloud",
        label: "クラウド",
        family: Family::Light,
        hue: 260.0,
        tint: 0.35,
    },
    MainColor {
        id: "sakura",
        label: "さくら",
        family: Family::Pastel,
        hue: 345.0,
        tint: 0.95,
    },
    MainColor {
        id: "lavender",
        label: "ラベンダー",
        family: Family::Pastel,
        hue: 265.0,
        tint: 0.85,
    },
    MainColor {
        id: "mint",
        label: "ミント",
        family: Family::Pastel,
        hue: 155.0,
        tint: 0.85,
    },
    MainColor {
        id: "sky",
        label: "スカイ",
        family: Family::Pastel,
        hue: 205.0,
        tint: 1.0,
    },
    MainColor {
        id: "peach",
        label: "ピーチ",
        family: Family::Pastel,
        hue: 25.0,
        tint: 0.95,
    },
    MainColor {
        id: "lemon",
        label: "レモン",
        family: Family::Pastel,
        hue: 50.0,
        tint: 0.85,
    },
];

pub(crate) const ACCENT_COLORS: [AccentColor; 18] = [
    AccentColor {
        id: "deep-blue",
        label: "ディープブルー",
        family: Family::Dark,
        hue: 218.0,
        saturation: 0.65,
        lightness: 0.42,
    },
    AccentColor {
        id: "emerald",
        label: "エメラルド",
        family: Family::Dark,
        hue: 160.0,
        saturation: 0.60,
        lightness: 0.40,
    },
    AccentColor {
        id: "crimson",
        label: "クリムゾン",
        family: Family::Dark,
        hue: 348.0,
        saturation: 0.68,
        lightness: 0.44,
    },
    AccentColor {
        id: "violet",
        label: "バイオレット",
        family: Family::Dark,
        hue: 268.0,
        saturation: 0.55,
        lightness: 0.46,
    },
    AccentColor {
        id: "amber",
        label: "アンバー",
        family: Family::Dark,
        hue: 38.0,
        saturation: 0.85,
        lightness: 0.48,
    },
    AccentColor {
        id: "teal",
        label: "ティール",
        family: Family::Dark,
        hue: 185.0,
        saturation: 0.60,
        lightness: 0.40,
    },
    AccentColor {
        id: "white",
        label: "ホワイト",
        family: Family::Light,
        hue: 220.0,
        saturation: 0.08,
        lightness: 0.95,
    },
    AccentColor {
        id: "silver",
        label: "シルバー",
        family: Family::Light,
        hue: 215.0,
        saturation: 0.10,
        lightness: 0.82,
    },
    AccentColor {
        id: "ice",
        label: "アイス",
        family: Family::Light,
        hue: 200.0,
        saturation: 0.40,
        lightness: 0.88,
    },
    AccentColor {
        id: "pearl",
        label: "パール",
        family: Family::Light,
        hue: 40.0,
        saturation: 0.30,
        lightness: 0.92,
    },
    AccentColor {
        id: "smoke",
        label: "スモーク",
        family: Family::Light,
        hue: 220.0,
        saturation: 0.07,
        lightness: 0.72,
    },
    AccentColor {
        id: "champagne",
        label: "シャンパン",
        family: Family::Light,
        hue: 45.0,
        saturation: 0.40,
        lightness: 0.86,
    },
    AccentColor {
        id: "sky",
        label: "スカイ",
        family: Family::Pastel,
        hue: 210.0,
        saturation: 1.0,
        lightness: 0.70,
    },
    AccentColor {
        id: "mint",
        label: "ミント",
        family: Family::Pastel,
        hue: 165.0,
        saturation: 0.70,
        lightness: 0.72,
    },
    AccentColor {
        id: "peach",
        label: "ピーチ",
        family: Family::Pastel,
        hue: 20.0,
        saturation: 0.90,
        lightness: 0.78,
    },
    AccentColor {
        id: "lavender",
        label: "ラベンダー",
        family: Family::Pastel,
        hue: 265.0,
        saturation: 0.75,
        lightness: 0.78,
    },
    AccentColor {
        id: "rose",
        label: "ローズ",
        family: Family::Pastel,
        hue: 340.0,
        saturation: 0.85,
        lightness: 0.76,
    },
    AccentColor {
        id: "lemon",
        label: "レモン",
        family: Family::Pastel,
        hue: 50.0,
        saturation: 0.90,
        lightness: 0.74,
    },
];

pub(crate) fn main_colors() -> &'static [MainColor] {
    &MAIN_COLORS
}

pub(crate) fn accent_colors() -> &'static [AccentColor] {
    &ACCENT_COLORS
}

pub(crate) fn find_main(id: &str) -> &'static MainColor {
    MAIN_COLORS
        .iter()
        .find(|color| color.id == id)
        .unwrap_or(&MAIN_COLORS[0])
}

pub(crate) fn find_accent(id: &str) -> &'static AccentColor {
    ACCENT_COLORS
        .iter()
        .find(|color| color.id == id)
        .unwrap_or(&ACCENT_COLORS[12])
}

#[derive(Clone, Copy)]
struct Template {
    hue: f32,
    bg: u32,
    glass: u32,
    panel: u32,
    panel_2: u32,
    panel_sidebar: u32,
    panel_header: u32,
    panel_card: u32,
    panel_inner: u32,
    panel_deep: u32,
    border: u32,
    border_strong: u32,
    line_soft: u32,
    highlight: u32,
    shadow: u32,
    scrim: u32,
    text: u32,
    muted: u32,
    muted_2: u32,
}

const DARK_TEMPLATE: Template = Template {
    hue: 220.0,
    bg: 0x07090d,
    glass: 0x0a101c61,
    panel: 0x121722,
    panel_2: 0x1a2130,
    panel_sidebar: 0x0b111b,
    panel_header: 0x101725,
    panel_card: 0x0d1420,
    panel_inner: 0x111927,
    panel_deep: 0x0a1019,
    border: 0x232b38,
    border_strong: 0x364256,
    line_soft: 0xffffff1b,
    highlight: 0xffffff14,
    shadow: 0x00000047,
    scrim: 0x05070acc,
    text: 0xeef3fb,
    muted: 0xa3adbd,
    muted_2: 0x768196,
};

const LIGHT_TEMPLATE: Template = Template {
    hue: 220.0,
    bg: 0xf1f3f6,
    glass: 0xf1f3f6d9,
    panel: 0xfbfcfe,
    panel_2: 0xe9edf3,
    panel_sidebar: 0xf7f8fb,
    panel_header: 0xffffff,
    panel_card: 0xffffff,
    panel_inner: 0xf3f5f9,
    panel_deep: 0xe7ebf1,
    border: 0xd7dde7,
    border_strong: 0xb7c2d2,
    line_soft: 0x0f172a1a,
    highlight: 0xffffff80,
    shadow: 0x0f172a1f,
    scrim: 0x05070a59,
    text: 0x171c24,
    muted: 0x4b5563,
    muted_2: 0x6b7688,
};

const PASTEL_TEMPLATE: Template = Template {
    hue: 205.0,
    bg: 0xd9e9f5,
    glass: 0xd9e9f5d9,
    panel: 0xe4f0f9,
    panel_2: 0xcfe2f1,
    panel_sidebar: 0xeaf4fb,
    panel_header: 0xf0f8fd,
    panel_card: 0xf8fcff,
    panel_inner: 0xddecf7,
    panel_deep: 0xc9deee,
    border: 0xb6d1e5,
    border_strong: 0x8fb5d1,
    line_soft: 0x0b122014,
    highlight: 0xffffff8c,
    shadow: 0x0b12201f,
    scrim: 0x05070a59,
    text: 0x1b2a38,
    muted: 0x43586a,
    muted_2: 0x64798c,
};

fn template_for(family: Family) -> Template {
    match family {
        Family::Dark => DARK_TEMPLATE,
        Family::Light => LIGHT_TEMPLATE,
        Family::Pastel => PASTEL_TEMPLATE,
    }
}

pub(crate) fn resolve(main_id: &str, accent_id: &str) -> Palette {
    let main = find_main(main_id);
    let accent = find_accent(accent_id);
    let base = template_for(main.family);
    let exact = (main.tint - 1.0).abs() < f32::EPSILON && (main.hue - base.hue).abs() < 0.5;
    let template = if exact {
        base
    } else {
        Template {
            bg: retint(base.bg, main.hue, main.tint),
            glass: base.glass,
            panel: retint(base.panel, main.hue, main.tint),
            panel_2: retint(base.panel_2, main.hue, main.tint),
            panel_sidebar: retint(base.panel_sidebar, main.hue, main.tint),
            panel_header: retint(base.panel_header, main.hue, main.tint),
            panel_card: retint(base.panel_card, main.hue, main.tint),
            panel_inner: retint(base.panel_inner, main.hue, main.tint),
            panel_deep: retint(base.panel_deep, main.hue, main.tint),
            border: retint(base.border, main.hue, main.tint),
            border_strong: retint(base.border_strong, main.hue, main.tint),
            text: retint(base.text, main.hue, main.tint),
            muted: retint(base.muted, main.hue, main.tint),
            muted_2: retint(base.muted_2, main.hue, main.tint),
            ..base
        }
    };

    let panel_card = template.panel_card;
    let surfaces = [panel_card, template.panel_inner, template.panel_2];
    let text = ensure_contrast(template.text, panel_card, 7.0);
    let muted = ensure_contrast(template.muted, panel_card, 4.5);
    let muted_2 = ensure_contrast(template.muted_2, panel_card, 3.0);
    let border = ensure_contrast(template.border, panel_card, 1.2);
    let border_strong = ensure_contrast(template.border_strong, panel_card, 1.8);

    let base_accent = hsl_to_rgb(accent.hue, accent.saturation, accent.lightness);
    let (accent_color, accent_soft, accent_soft_hover) =
        safe_tone(base_accent, panel_card, &surfaces, 0.17, 0.26);
    let accent_fill = if contrast_ratio(base_accent, panel_card) >= 1.3 {
        base_accent
    } else {
        accent_color
    };
    let accent_border = ensure_contrast(base_accent, panel_card, 3.0);
    let on_accent = best_text(accent_fill);

    let (green, green_soft, _) = safe_tone(
        hsl_to_rgb(170.6, 0.769, 0.643),
        panel_card,
        &surfaces,
        0.15,
        0.15,
    );
    let (amber, amber_soft, _) = safe_tone(
        hsl_to_rgb(42.9, 0.96, 0.563),
        panel_card,
        &surfaces,
        0.15,
        0.15,
    );
    let (red, red_soft, _) = safe_tone(
        hsl_to_rgb(351.0, 0.94, 0.714),
        panel_card,
        &surfaces,
        0.15,
        0.15,
    );
    let red_border = mix(panel_card, red, 0.30);

    let glass = if exact {
        template.glass
    } else {
        with_alpha(
            retint(template.glass & 0xffffff, main.hue, main.tint),
            (template.glass & 0xff) as u8,
        )
    };

    Palette {
        is_dark: main.family.is_dark(),
        is_pastel: main.family == Family::Pastel,
        bg: template.bg,
        glass,
        panel: template.panel,
        panel_2: template.panel_2,
        panel_sidebar: template.panel_sidebar,
        panel_header: template.panel_header,
        panel_card,
        panel_inner: template.panel_inner,
        panel_deep: template.panel_deep,
        border,
        border_strong,
        line_soft: template.line_soft,
        highlight: template.highlight,
        shadow: template.shadow,
        scrim: template.scrim,
        text,
        muted,
        muted_2,
        accent: accent_color,
        accent_fill,
        accent_soft,
        accent_soft_hover,
        accent_border,
        on_accent,
        green,
        green_soft,
        amber,
        amber_soft,
        red,
        red_soft,
        red_border,
    }
}

fn safe_tone(
    base: u32,
    panel_card: u32,
    surfaces: &[u32],
    soft_weight: f32,
    _hover_weight: f32,
) -> (u32, u32, u32) {
    let mut color = ensure_contrast_vs_all(base, surfaces, 4.5);
    let mut soft = mix(panel_card, color, soft_weight);
    let mut hover = mix(panel_card, color, 0.26);
    for _ in 0..24 {
        let mut backgrounds = Vec::with_capacity(surfaces.len() + 2);
        backgrounds.extend_from_slice(surfaces);
        backgrounds.push(soft);
        backgrounds.push(hover);
        let next = ensure_contrast_vs_all(color, &backgrounds, 4.5);
        if next == color {
            break;
        }
        color = next;
        soft = mix(panel_card, color, soft_weight);
        hover = mix(panel_card, color, 0.26);
    }
    (color, soft, hover)
}

fn ensure_contrast_vs_all(fg: u32, backgrounds: &[u32], min: f32) -> u32 {
    let worst = backgrounds
        .iter()
        .copied()
        .min_by(|a, b| {
            contrast_ratio(fg, *a)
                .partial_cmp(&contrast_ratio(fg, *b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0x000000);
    ensure_contrast(fg, worst, min)
}

fn best_text(fill: u32) -> u32 {
    const DARK_TEXT: u32 = 0x101418;
    const LIGHT_TEXT: u32 = 0xffffff;
    if contrast_ratio(DARK_TEXT, fill) >= contrast_ratio(LIGHT_TEXT, fill) {
        DARK_TEXT
    } else {
        LIGHT_TEXT
    }
}

pub(crate) fn preview_main(color: &MainColor) -> u32 {
    let base = template_for(color.family);
    if (color.tint - 1.0).abs() < f32::EPSILON && (color.hue - base.hue).abs() < 0.5 {
        base.panel_2
    } else {
        retint(base.panel_2, color.hue, color.tint)
    }
}

pub(crate) fn preview_accent(color: &AccentColor) -> u32 {
    hsl_to_rgb(color.hue, color.saturation, color.lightness)
}

fn retint(color: u32, hue: f32, tint: f32) -> u32 {
    let (_, saturation, lightness) = rgb_to_hsl(color);
    hsl_to_rgb(hue, (saturation * tint).clamp(0.0, 1.0), lightness)
}

pub(crate) fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> u32 {
    let h = hue.rem_euclid(360.0) / 60.0;
    let s = saturation.clamp(0.0, 1.0);
    let l = lightness.clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let channel = |value: f32| ((value + m) * 255.0).round().clamp(0.0, 255.0) as u32;
    (channel(r) << 16) | (channel(g) << 8) | channel(b)
}

pub(crate) fn rgb_to_hsl(color: u32) -> (f32, f32, f32) {
    let r = ((color >> 16) & 0xff) as f32 / 255.0;
    let g = ((color >> 8) & 0xff) as f32 / 255.0;
    let b = (color & 0xff) as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / d).rem_euclid(6.0) * 60.0
    } else if (max - g).abs() < f32::EPSILON {
        ((b - r) / d + 2.0) * 60.0
    } else {
        ((r - g) / d + 4.0) * 60.0
    };
    (h, s.clamp(0.0, 1.0), l)
}

fn channel_luminance(value: u32) -> f32 {
    let c = value as f32 / 255.0;
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn relative_luminance(color: u32) -> f32 {
    0.2126 * channel_luminance((color >> 16) & 0xff)
        + 0.7152 * channel_luminance((color >> 8) & 0xff)
        + 0.0722 * channel_luminance(color & 0xff)
}

pub(crate) fn contrast_ratio(a: u32, b: u32) -> f32 {
    let a = relative_luminance(a);
    let b = relative_luminance(b);
    let (light, dark) = if a >= b { (a, b) } else { (b, a) };
    (light + 0.05) / (dark + 0.05)
}

pub(crate) fn ensure_contrast(fg: u32, bg: u32, min: f32) -> u32 {
    if contrast_ratio(fg, bg) >= min {
        return fg;
    }
    let (h, s, l) = rgb_to_hsl(fg);
    let dark_first = contrast_ratio(0x000000, bg) >= contrast_ratio(0xffffff, bg);
    let (mut lo, mut hi) = if dark_first { (0.0, l) } else { (l, 1.0) };
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let candidate = hsl_to_rgb(h, s, mid);
        if contrast_ratio(candidate, bg) >= min {
            if dark_first {
                lo = mid;
            } else {
                hi = mid;
            }
        } else if dark_first {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let candidate = hsl_to_rgb(h, s, if dark_first { lo } else { hi });
    if contrast_ratio(candidate, bg) >= min {
        candidate
    } else if dark_first {
        0x000000
    } else {
        0xffffff
    }
}

pub(crate) fn lerp_color(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let from = ((from >> shift) & 0xff) as f32;
        let to = ((to >> shift) & 0xff) as f32;
        (((from + (to - from) * t).round() as u32) & 0xff) << shift
    };
    channel(16) | channel(8) | channel(0)
}

pub(crate) fn mix(base: u32, color: u32, weight: f32) -> u32 {
    lerp_color(base, color, weight)
}

pub(crate) fn with_alpha(color: u32, alpha: u8) -> u32 {
    (color & 0xffffff00) | u32::from(alpha)
}

impl Palette {
    pub(crate) fn scene_color(&self, world: &str) -> u32 {
        let mut hash = 0u32;
        for byte in world.as_bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(u32::from(*byte));
        }
        let hue = (hash % 360) as f32;
        if self.is_dark {
            hsl_to_rgb(hue, 0.35, 0.15)
        } else if self.is_pastel {
            hsl_to_rgb(hue, 0.35, 0.84)
        } else {
            hsl_to_rgb(hue, 0.25, 0.90)
        }
    }
}

pub(crate) fn apply_component_theme(palette: Palette, cx: &mut App) {
    let mode = if palette.is_dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.colors.background = rgb(palette.bg).into();
        theme.colors.foreground = rgb(palette.text).into();
        theme.colors.border = rgb(palette.border).into();
        theme.colors.muted = rgb(palette.panel_inner).into();
        theme.colors.muted_foreground = rgb(palette.muted).into();
        theme.colors.input = rgb(palette.border).into();
        theme.colors.ring = rgb(palette.accent).into();
        theme.colors.selection = rgb(palette.accent_soft).into();
        theme.colors.caret = rgb(palette.accent).into();
        theme.colors.popover = rgb(palette.panel_card).into();
        theme.colors.popover_foreground = rgb(palette.text).into();
        theme.colors.accent = rgb(palette.accent_soft).into();
        theme.colors.accent_foreground = rgb(palette.accent).into();
        theme.colors.link = rgb(palette.accent).into();
        theme.colors.list = rgb(palette.panel_card).into();
        theme.colors.list_hover = rgb(palette.panel_inner).into();
        theme.colors.list_active = rgb(palette.accent_soft).into();
        theme.colors.list_active_border = rgb(palette.accent_border).into();
        theme.colors.list_even = rgb(palette.panel_2).into();
        theme.colors.list_head = rgb(palette.panel_header).into();
        theme.colors.button = rgb(palette.panel_2).into();
        theme.colors.button_hover = rgb(palette.panel_inner).into();
        theme.colors.button_active = rgb(palette.panel).into();
        theme.colors.button_foreground = rgb(palette.text).into();
        theme.colors.button_primary = rgb(palette.accent_fill).into();
        theme.colors.button_primary_hover =
            rgb(mix(palette.accent_fill, palette.on_accent, 0.10)).into();
        theme.colors.button_primary_active =
            rgb(mix(palette.accent_fill, palette.on_accent, 0.18)).into();
        theme.colors.button_primary_foreground = rgb(palette.on_accent).into();
        theme.colors.primary = rgb(palette.accent_fill).into();
        theme.colors.primary_foreground = rgb(palette.on_accent).into();
        theme.colors.danger = rgb(palette.red).into();
        theme.colors.warning = rgb(palette.amber).into();
        theme.colors.success = rgb(palette.green).into();
        theme.colors.button_danger = rgb(palette.red).into();
        theme.colors.button_danger_foreground = rgb(best_text(palette.red)).into();
        theme.colors.button_warning = rgb(palette.amber).into();
        theme.colors.button_warning_foreground = rgb(best_text(palette.amber)).into();
        theme.colors.button_success = rgb(palette.green).into();
        theme.colors.button_success_foreground = rgb(best_text(palette.green)).into();
        theme.colors.scrollbar = rgb(palette.border_strong).into();
        theme.colors.scrollbar_thumb = rgb(palette.muted_2).into();
        theme.colors.scrollbar_thumb_hover = rgb(palette.muted).into();
        theme.overlay = rgba(palette.scrim).into();
        theme.notification.placement = Anchor::BottomRight;
        theme.notification.width = px(340.);
        theme.notification.margins = Edges {
            top: px(16.),
            right: px(16.),
            bottom: px(16.),
            left: px(16.),
        };
    });
    // Root paints `tokens.background` across the window. Keep it transparent
    // so the window glass reaches the compositor; `colors.background` stays
    // opaque because light-mode inputs use it as their fill.
    Theme::global_mut(cx).tokens.background = transparent_black().into();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_contrast(fg: u32, bg: u32, min: f32, label: &str) {
        let ratio = contrast_ratio(fg, bg);
        assert!(
            ratio >= min,
            "{label}: {ratio:.2} < {min} (fg #{fg:06x}, bg #{bg:06x})"
        );
    }

    #[test]
    fn hsl_roundtrip() {
        for color in [
            0x000000, 0xffffff, 0x66b3ff, 0x0d1420, 0xf1f3f6, 0xd9e9f5, 0xfb7185,
        ] {
            let (h, s, l) = rgb_to_hsl(color);
            assert_eq!(hsl_to_rgb(h, s, l), color, "roundtrip #{color:06x}");
        }
    }

    #[test]
    fn contrast_ratio_known_pairs() {
        assert!((contrast_ratio(0x000000, 0xffffff) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(0xffffff, 0xffffff) - 1.0).abs() < 0.001);
        assert!(contrast_ratio(0x66b3ff, 0x0d1420) > 4.5);
    }

    #[test]
    fn ensure_contrast_reaches_target() {
        let adjusted = ensure_contrast(0x66b3ff, 0xffffff, 4.5);
        assert!(contrast_ratio(adjusted, 0xffffff) >= 4.5);
        let (h, _, _) = rgb_to_hsl(0x66b3ff);
        let (h2, _, _) = rgb_to_hsl(adjusted);
        assert!((h - h2).abs() < 2.0 || adjusted == 0x000000);
    }

    #[test]
    fn catalog_has_18_per_axis_and_6_per_family() {
        assert_eq!(main_colors().len(), 18);
        assert_eq!(accent_colors().len(), 18);
        for family in [Family::Dark, Family::Light, Family::Pastel] {
            assert_eq!(
                main_colors()
                    .iter()
                    .filter(|color| color.family == family)
                    .count(),
                6
            );
            assert_eq!(
                accent_colors()
                    .iter()
                    .filter(|color| color.family == family)
                    .count(),
                6
            );
        }
        let mut ids: Vec<&str> = main_colors().iter().map(|color| color.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 18);
    }

    #[test]
    fn resolve_falls_back_to_defaults() {
        let fallback = resolve("missing", "missing");
        let default = resolve("charcoal", "sky");
        assert_eq!(fallback, default);
        assert_eq!(resolve("", ""), default);
    }

    #[test]
    fn default_matches_current_palette() {
        let palette = resolve("charcoal", "sky");
        assert_eq!(palette.panel_card, 0x0d1420);
        assert_eq!(palette.panel_header, 0x101725);
        assert_eq!(palette.panel_sidebar, 0x0b111b);
        assert_eq!(palette.panel_inner, 0x111927);
        assert_eq!(palette.text, 0xeef3fb);
        assert_eq!(palette.accent, 0x66b3ff);
        let (r, g, b) = (
            (palette.accent_soft >> 16) & 0xff,
            (palette.accent_soft >> 8) & 0xff,
            palette.accent_soft & 0xff,
        );
        assert!((r as i32 - 0x1a).abs() <= 4);
        assert!((g as i32 - 0x2f).abs() <= 4);
        assert!((b as i32 - 0x47).abs() <= 4);
    }

    #[test]
    fn all_pairs_are_readable() {
        for main in main_colors() {
            for accent in accent_colors() {
                let palette = resolve(main.id, accent.id);
                let label = format!("{}+{}", main.id, accent.id);
                assert_contrast(palette.text, palette.panel_card, 7.0, &label);
                assert_contrast(palette.muted, palette.panel_card, 4.5, &label);
                assert_contrast(palette.muted_2, palette.panel_card, 3.0, &label);
                assert_contrast(palette.accent, palette.panel_card, 4.5, &label);
                assert_contrast(palette.accent, palette.panel_inner, 4.5, &label);
                assert_contrast(palette.accent, palette.panel_2, 4.5, &label);
                assert_contrast(palette.accent, palette.accent_soft, 4.5, &label);
                assert_contrast(palette.accent, palette.accent_soft_hover, 4.5, &label);
                assert_contrast(palette.red, palette.red_soft, 4.5, &label);
                assert_contrast(palette.amber, palette.amber_soft, 4.5, &label);
                assert_contrast(palette.green, palette.green_soft, 4.5, &label);
                assert_contrast(palette.on_accent, palette.accent_fill, 4.5, &label);
                assert_contrast(palette.border, palette.panel_card, 1.2, &label);
                assert_contrast(palette.border_strong, palette.panel_card, 1.8, &label);
            }
        }
    }

    #[test]
    fn no_hardcoded_palette_colors_outside_palette() {
        use std::path::Path;

        fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read source directory") {
                let path = entry.expect("source entry").path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect(&src, &mut files);
        for file in files {
            if file.ends_with("ui/palette.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("read source file");
            for (index, line) in text.lines().enumerate() {
                let bad = line.contains("rgb(0x")
                    || (line.contains("rgba(0x") && !line.contains("rgba(0x00000000)"));
                assert!(
                    !bad,
                    "{}:{}: hardcoded palette colour: {}",
                    file.display(),
                    index + 1,
                    line.trim()
                );
            }
        }
    }

    #[test]
    fn scene_color_follows_the_family() {
        for main in main_colors() {
            let palette = resolve(main.id, "sky");
            let (_, _, lightness) = rgb_to_hsl(palette.scene_color("A quiet forest"));
            if main.family == Family::Dark {
                assert!(lightness < 0.25, "{}: {lightness}", main.id);
            } else {
                assert!(lightness > 0.75, "{}: {lightness}", main.id);
            }
        }
    }
}
