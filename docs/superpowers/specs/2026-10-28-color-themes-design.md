# Color themes — design

## Intent

The app is dark-only. Every colour is a compile-time constant in
`crates/ui/src/ui/theme.rs`, and the embedded `gpui-component` widgets are
forced to `ThemeMode::Dark`. Users cannot change the overall surface colour or
the accent, even though the glass shell makes the window sit next to apps with
very different palettes.

This change adds two independent colour choices in Settings:

- **Main colour** — the background, panels, cards, text, borders, and effects.
- **Accent colour** — the colour currently hardcoded as blue (`BLUE`,
  `0x66b3ff`) for selections, links, focus rings, progress, and primary
  actions.

Each choice has three families — dark (黒系), light (白系), and pastel
(パステルカラー系) — with six colours per family. All 18 × 18 combinations are
allowed and take effect immediately.

The default (`charcoal` main + `sky` accent) reproduces today's appearance.

## Goals

- Two settings, chosen independently, with 18 options each (6 dark, 6 light,
  6 pastel), grouped by family in the UI.
- A complete, coherent surface palette per main colour: every surface, text
  tone, border, shadow, and overlay is derived from the selection.
- A complete accent set per accent colour: text/icon accent, soft fill,
  hover fill, border, filled-button fill, and on-accent text.
- Light and pastel mains produce a genuine light UI: dark text, light cards,
  and matching `gpui-component` widgets (buttons, inputs, switches, scrollbars,
  dialogs).
- Any pairing stays readable. Accent lightness is adjusted automatically when
  the chosen accent would be lost against the current surfaces; the swatch
  still shows the colour the user picked.
- The setting persists per user (SQLite settings JSON) and survives restarts.
- No hardcoded palette colours remain outside the palette module, so a future
  colour cannot silently break light mode.

## Non-goals

- Free-form colour picking (hex/RGB wheel), per-window themes, or user theme
  import/export.
- Following the OS light/dark preference automatically.
- Recolouring photo content, thumbnails, or the world-name placeholder hues
  beyond tinting the placeholder surfaces.
- Changing layout metrics, spacing, motion, or the glass/blur behaviour itself.
- New accessibility settings beyond contrast-safe derived colours.

## Anti-goals

- A light main with dark cards, white-on-white text, or dark `gpui-component`
  inputs left behind.
- A pairing that makes the accent invisible (white accent on a white card,
  black accent on a black card) with no correction.
- A theme change that resets unrelated settings or requires a restart.
- Exploding the palette into 55 hand-maintained values per colour.

## Current state (what the change touches)

- `crates/ui/src/ui/theme.rs` owns 55 `const` tokens. Screens consume them via
  `use crate::ui::theme::*` and call `rgb(TOKEN)` / `rgba(TOKEN)`.
- About 150 references use those constants; another ~150 hex literals live
  outside `theme.rs` (white/black overlay alphas, accent alphas, danger
  tints, gradients, shadows, and the `scene_color` placeholder palette).
- `crates/ui/src/main.rs` forces `ThemeMode::Dark` and clears
  `tokens.background` so the window glass reaches the compositor.
- `gpui-component` widgets read their own `Theme` global (`colors` for inputs,
  `tokens` for buttons); those tokens are dark today.
- `AppSettings` already persists UI preferences in one JSON blob and screens
  save immediately on change. A theme change can follow the same path.
- `feat/ui-notifications` is the base of this work. The colour-theme branch
  is stacked on it and the final PR carries both sets of commits, because the
  user asked for one combined PR.

## Approaches considered

| Approach | What it does | Trade-off |
| --- | --- | --- |
| **A. Runtime palette + derivation (chosen)** | Move colours into a `Palette` value, derive each colour family from one hand-tuned template plus hue/tint, and compute contrast-safe accent/status tones. | One source of truth; adding a colour is one catalog row; all pairings are testable. Less art-directed than hand-tuning every colour. |
| B. Hand-write every preset | Define all tokens for all 36 colours by hand. | Best per-colour control, but 36 × ~30 values to maintain and easy to break one state. Rejected: the request asks for many colours. |
| C. Move everything onto `gpui-component` tokens | Delete the app palette and use only component theme tokens. | Small diff, but the 30-token design (glass, inset highlight, sidebar/header/card hierarchy) cannot be expressed, and the current look would change. Rejected. |

A was chosen because "many colours" and "any combination" require a generator
with enforced contrast, not a larger hand-written table.

## Design

### 1. Data model

`crates/ui/src/ui/palette.rs` (new) owns everything colour-related.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    pub(crate) is_dark: bool,

    // Surfaces
    pub(crate) bg: u32,
    pub(crate) glass: u32,        // packed alpha in the low byte, used with rgba()
    pub(crate) panel: u32,
    pub(crate) panel_2: u32,
    pub(crate) panel_sidebar: u32,
    pub(crate) panel_header: u32,
    pub(crate) panel_card: u32,
    pub(crate) panel_inner: u32,
    pub(crate) panel_deep: u32,

    // Lines and effects
    pub(crate) border: u32,
    pub(crate) border_strong: u32,
    pub(crate) line_soft: u32,    // packed alpha
    pub(crate) highlight: u32,    // packed alpha, top inset highlight
    pub(crate) shadow: u32,       // packed alpha, black
    pub(crate) scrim: u32,        // packed alpha, modal backdrop

    // Text
    pub(crate) text: u32,
    pub(crate) muted: u32,
    pub(crate) muted_2: u32,

    // Accent
    pub(crate) accent: u32,            // contrast-safe text/icon accent
    pub(crate) accent_fill: u32,       // filled control background
    pub(crate) accent_soft: u32,       // selected row / chip background
    pub(crate) accent_soft_hover: u32,
    pub(crate) accent_border: u32,
    pub(crate) on_accent: u32,         // text on accent_fill

    // Status
    pub(crate) green: u32,
    pub(crate) green_soft: u32,
    pub(crate) amber: u32,
    pub(crate) amber_soft: u32,
    pub(crate) red: u32,
    pub(crate) red_soft: u32,
    pub(crate) red_border: u32,
}
```

Dead legacy tokens are removed: `SIDEBAR`, `PANEL_SIDEBAR_DEEP`,
`PANEL_INNER_DEEP`, `PANEL_RAISED`. All remaining tokens map as:

| Old constant | New field |
| --- | --- |
| `BG` | `palette.bg` |
| `GLASS_WINDOW` | `palette.glass` |
| `PANEL` | `palette.panel` |
| `PANEL_2` | `palette.panel_2` |
| `PANEL_SIDEBAR` | `palette.panel_sidebar` |
| `PANEL_HEADER` | `palette.panel_header` |
| `PANEL_CARD` | `palette.panel_card` |
| `PANEL_INNER` | `palette.panel_inner` |
| `PANEL_DEEP` | `palette.panel_deep` |
| `BORDER` / `BORDER_STRONG` | `border` / `border_strong` |
| `TEXT` / `MUTED` / `MUTED_2` | `text` / `muted` / `muted_2` |
| `BLUE` | `accent` |
| `ACCENT_SOFT` | `accent_soft` |
| `GREEN` / `AMBER` / `RED` | `green` / `amber` / `red` |

Colours stay `u32` in `0xRRGGBB` (or `0xRRGGBBAA` when alpha is packed) so the
existing `rgb()` / `rgba()` call style keeps working.

### 2. Catalog

Two static tables, each with 18 entries. IDs are scoped to their axis, so
`sky` may exist in both tables.

```rust
pub(crate) enum Family { Dark, Light, Pastel } // 黒系 / 白系 / パステルカラー系

pub(crate) struct MainColor {
    pub id: &'static str,
    pub label: &'static str,
    pub family: Family,
    pub hue: f32,   // 0..360
    pub tint: f32,  // saturation multiplier
}

pub(crate) struct AccentColor {
    pub id: &'static str,
    pub label: &'static str,
    pub family: Family,
    pub hue: f32,
    pub saturation: f32,
    pub lightness: f32,
}
```

#### Main colours

| id | label | family | hue | tint |
| --- | --- | --- | --- | --- |
| `charcoal` | チャコール | dark | 220 | 1.00 (default) |
| `midnight` | ミッドナイト | dark | 228 | 1.15 |
| `forest` | フォレスト | dark | 155 | 0.85 |
| `plum` | プラム | dark | 285 | 0.90 |
| `wine` | ワイン | dark | 350 | 0.85 |
| `sepia` | セピア | dark | 35 | 0.80 |
| `paper` | ペーパー | light | 220 | 1.00 |
| `ivory` | アイボリー | light | 45 | 1.20 |
| `mist` | ミスト | light | 205 | 1.65 |
| `sage` | セージ | light | 145 | 1.50 |
| `blush` | ブラッシュ | light | 345 | 1.40 |
| `cloud` | クラウド | light | 260 | 1.50 |
| `sakura` | さくら | pastel | 345 | 0.95 |
| `lavender` | ラベンダー | pastel | 265 | 0.85 |
| `mint` | ミント | pastel | 155 | 0.85 |
| `sky` | スカイ | pastel | 205 | 1.00 |
| `peach` | ピーチ | pastel | 25 | 0.95 |
| `lemon` | レモン | pastel | 50 | 0.85 |

Light-family colours also apply a small lightness offset after retinting:
ivory `+1.6%`, mist `-0.5%`, sage `-1.6%`, blush `+0.9%`, cloud `-0.9%`
(paper stays exact). This keeps the six white-family backgrounds visibly
distinct without leaving the light range; `light_family_previews_are_distinct`
pins a minimum RGB distance of 5.

#### Accent colours

| id | label | family | hue | sat | light |
| --- | --- | --- | --- | --- | --- |
| `deep-blue` | ディープブルー | dark | 218 | 0.65 | 0.42 |
| `emerald` | エメラルド | dark | 160 | 0.60 | 0.40 |
| `crimson` | クリムゾン | dark | 348 | 0.68 | 0.44 |
| `violet` | バイオレット | dark | 268 | 0.55 | 0.46 |
| `amber` | アンバー | dark | 38 | 0.85 | 0.48 |
| `teal` | ティール | dark | 185 | 0.60 | 0.40 |
| `white` | ホワイト | light | 220 | 0.08 | 0.95 |
| `silver` | シルバー | light | 215 | 0.10 | 0.82 |
| `ice` | アイス | light | 200 | 0.40 | 0.88 |
| `pearl` | パール | light | 40 | 0.30 | 0.92 |
| `smoke` | スモーク | light | 220 | 0.07 | 0.72 |
| `champagne` | シャンパン | light | 45 | 0.40 | 0.86 |
| `sky` | スカイ | pastel | 210 | 1.00 | 0.70 (default, `#66b3ff`) |
| `mint` | ミント | pastel | 165 | 0.70 | 0.72 |
| `peach` | ピーチ | pastel | 20 | 0.90 | 0.78 |
| `lavender` | ラベンダー | pastel | 265 | 0.75 | 0.78 |
| `rose` | ローズ | pastel | 340 | 0.85 | 0.76 |
| `lemon` | レモン | pastel | 50 | 0.90 | 0.74 |

### 3. Derivation

All maths is pure and unit-testable. Implement in `palette.rs`:

- `hsl_to_rgb(h, s, l) -> u32`
- `rgb_to_hsl(u32) -> (f32, f32, f32)`
- `relative_luminance(u32) -> f32` and `contrast_ratio(a, b) -> f32`
  (WCAG 2.x)
- `ensure_contrast(fg, bg, min_ratio) -> u32`: binary-searches HSL lightness,
  keeping hue and saturation, moving away from the background's luminance.
  Returns `fg` unchanged when it already passes.
- `mix(base, color, weight) -> u32`: `weight` is the share of `color`
  (`mix(panel_card, accent, 0.17)` is 17 % accent, 83 % card). This wraps the
  existing `lerp_color(base, color, weight)` and is moved here.
- `with_alpha(color, a: u8) -> u32` (packs an opaque `0xRRGGBB` into
  `0xRRGGBBAA`; callers never pass an already-packed colour)

#### Main palette: template + retint

Three hand-tuned templates hold the literal surface values, one per family.
`charcoal`, `paper`, and `sky` are the templates themselves; every other main
colour is the template with hue replaced and saturation multiplied by `tint`.

Why templates: the dark template is today's palette, so the default is
byte-exact. Light and pastel are hand-authored once, then hue-rotated.

**Dark template (current values, hue 220)**

| token | value |
| --- | --- |
| bg | `0x07090d` |
| glass | `0x0a101c61` |
| panel | `0x121722` |
| panel_2 | `0x1a2130` |
| panel_sidebar | `0x0b111b` |
| panel_header | `0x101725` |
| panel_card | `0x0d1420` |
| panel_inner | `0x111927` |
| panel_deep | `0x0a1019` |
| border | `0x232b38` |
| border_strong | `0x364256` |
| line_soft | `0xffffff1b` |
| highlight | `0xffffff14` |
| text | `0xeef3fb` |
| muted | `0xa3adbd` |
| muted_2 | `0x768196` |
| shadow | `0x00000047` |
| scrim | `0x05070acc` |

**Light template (`paper`, hue 220, low saturation)**

| token | value |
| --- | --- |
| bg | `0xf1f3f6` |
| glass | `0xf1f3f6d9` |
| panel | `0xfbfcfe` |
| panel_2 | `0xe9edf3` |
| panel_sidebar | `0xf7f8fb` |
| panel_header | `0xffffff` |
| panel_card | `0xffffff` |
| panel_inner | `0xf3f5f9` |
| panel_deep | `0xe7ebf1` |
| border | `0xd7dde7` |
| border_strong | `0xb7c2d2` |
| line_soft | `0x0f172a1a` |
| highlight | `0xffffff80` |
| text | `0x171c24` |
| muted | `0x4b5563` |
| muted_2 | `0x6b7688` |
| shadow | `0x0f172a1f` |
| scrim | `0x05070a59` |

**Pastel template (`sky`, hue 205)**

| token | value |
| --- | --- |
| bg | `0xd9e9f5` |
| glass | `0xd9e9f5d9` |
| panel | `0xe4f0f9` |
| panel_2 | `0xcfe2f1` |
| panel_sidebar | `0xeaf4fb` |
| panel_header | `0xf0f8fd` |
| panel_card | `0xf8fcff` |
| panel_inner | `0xddecf7` |
| panel_deep | `0xc9deee` |
| border | `0xb6d1e5` |
| border_strong | `0x8fb5d1` |
| line_soft | `0x0b122014` |
| highlight | `0xffffff8c` |
| text | `0x1b2a38` |
| muted | `0x43586a` |
| muted_2 | `0x64798c` |
| shadow | `0x0b12201f` |
| scrim | `0x05070a59` |

Retint rules:

1. Convert each opaque token to HSL.
2. Set hue to the catalog hue; multiply saturation by `tint` (clamp 1.0).
3. Keep lightness. Convert back to RGB.
4. `line_soft`, `highlight`, `shadow`, `scrim` are neutral overlays: keep them
   except `shadow`/`scrim`, which may take the new hue at their existing alpha.
5. `glass` is the template glass RGB retinted, with its alpha byte kept.
   (The dark default must stay `0x0a101c61`; deriving from `bg` would change
   the default window tint.)
6. Run the contrast guards below on text, muted tones, borders, and accent.

The light and pastel templates are deliberately authored with dark text and
light cards. `is_dark` is true only for the dark family.

#### Accent derivation

```
base      = hsl(entry)                       // the swatch colour
surfaces  = [panel_card, panel_inner, panel_2]
accent    = ensure_contrast_vs_all(base, surfaces, 4.5)
repeat 4 times:
    soft       = mix(panel_card, accent, 0.17)
    soft_hover = mix(panel_card, accent, 0.26)
    accent     = ensure_contrast(accent, soft, 4.5)
    accent     = ensure_contrast(accent, soft_hover, 4.5)
accent_border = ensure_contrast(base, panel_card, 3.0)
accent_fill   = if contrast(base, panel_card) >= 1.3 { base } else { accent }
on_accent     = black_or_white_with_best_contrast(accent_fill)
```

The loop re-checks both soft fills after each lightness move because a darker
accent also darkens the fill behind it; four passes converge in practice.
`accent` is what text, icons, links, focus rings, and progress use. `base`
stays the visible swatch and the fill of primary controls when it is
distinguishable from the surface. White accent on a white card becomes grey
for text while its swatch remains white; the Settings card explains this.

#### Status colours

Green, amber, and red keep their hue (status must not follow the main hue):

- base: green `HSL(170, 0.75, 0.62)`, amber `HSL(42, 0.95, 0.58)`,
  red `HSL(350, 0.85, 0.64)`.
- Each is passed through the same soft-fill loop as the accent:
  `ensure_contrast_vs_all(base, surfaces, 4.5)`, then four passes of
  `soft = mix(panel_card, status, 0.15)` and
  `status = ensure_contrast(status, soft, 4.5)`. Status text sits on its soft
  fill (badges, warning boxes, danger buttons), so both pairs must pass.
- `red_border = mix(panel_card, red, 0.30)` after the loop.
- The old `#2a1414` warning box becomes `red_soft`; `#ffd9df` danger hover
  text becomes `text`.
- On light mains the danger/warning text is therefore noticeably darker than
  today's `#fb7185`; that is required for 4.5:1 on a pale red fill.

#### Placeholder tint

`scene_color(world)` moves to `Palette::scene_color(world)`. It hashes the
world name to a hue offset and returns a surface tint appropriate to the
family (dark: L≈0.15; light: L≈0.90; pastel: L≈0.84) so empty thumbnails do
not punch dark holes in light themes.

### 4. Applying the palette

`Palette` lives on `App`:

```rust
pub(crate) struct App {
    pub(crate) palette: Palette,
    // ...
}
```

- `App::new` resolves the saved choice before the first render.
- Screens use `let p = self.palette;` at the top of a render helper and
  `rgb(p.panel_card)` etc.
- Free helpers that build `Div`s take `&Palette`:
  `glass_panel(&Palette)`, `dialog_panel(&Palette)`, `chip(&Palette, ...)`,
  `status_progress_bar(&Palette, ...)`, `manual_option_row(&Palette, ...)`,
  `update_downloading_view(&Palette, ...)`. Their call sites are few.
- No GPUI global, no thread-local. The palette is explicit and testable.

#### `gpui-component` mapping

`apply_component_theme(palette, cx)` in `palette.rs` runs on startup and on
every change:

1. `Theme::change(if palette.is_dark { Dark } else { Light }, None, cx)`.
2. `Theme::update(cx, |theme| { ... })` to overwrite the fields the app's
   widgets actually read. `Theme::update` reconciles `colors` and `tokens`,
   rebuilds the `gpui-base` projection (scrollbars, resize handles), and
   refreshes windows. A bare `global_mut` would leave that projection stale.
3. Mapping:
   - window: `background` = `bg` (opaque; it is the light-mode input fill),
     `foreground` = `text`, `border` = `border`, `muted_foreground` = `muted`,
     `muted` = `panel_inner`.
   - inputs: `input` = `border` (in `gpui-component`, `input` is the **border**
     colour; dark mode derives the fill from it at 30 % opacity, light mode
     uses `background`), `ring` = `accent`, `selection` = `accent_soft`,
     `caret` = `accent`, `popover` = `panel_card`,
     `popover_foreground` = `text`. The notes `Textarea` falls back to
     `input_background()` because `highlight_theme.editor_background` is
     `None`, so no highlight-theme override is needed.
   - buttons: `button` = `panel_2`, `button_hover` = `accent_soft`,
     `button_active` = `accent_soft_hover`, `button_foreground` = `text`,
     `button_primary` = `accent_fill`. Hover and active move the fill away
     from the mid-tone (8 % / 14 % toward white when `on_accent` is dark,
     toward black when it is white) so one foreground can pass 4.5:1 on all
     three states; that shared foreground is the best of black/white against
     the base, hover, and active fills.
     `primary` = `accent_fill`, `primary_foreground` = `on_accent`.
   - semantics: `danger`/`warning`/`success` and their buttons =
     `red`/`amber`/`green` with `*_soft` backgrounds.
   - lists: `list` = `panel_card`, `list_hover` = `panel_inner`,
     `list_active` = `accent_soft`, `list_active_border` = `accent_border`,
     `list_even` = `panel_2`, `list_head` = `panel_header`.
   - `accent` = `accent_soft`, `accent_foreground` = `accent`,
     `link` = `accent`, `link_hover` = `accent`, `scrollbar` = `border_strong`,
     `scrollbar_thumb` = `muted_2`, `shadow` = true.
4. Inside the same update, clear `tokens.background` to transparent so the
   window glass still reaches the compositor. Do **not** clear
   `colors.background`: light-mode inputs read it as their fill.
5. `Theme::update` already refreshes windows; an extra `cx.notify()` on the
   owning `App` covers the app's own paint.

The mapping only covers fields the app's widget set reads. Unmapped fields keep
the light/dark defaults from step 1. As a consequence, widget colours move
from the `gpui-component` defaults to palette tokens (text, border, input
fill, primary buttons); the app's own surfaces stay byte-identical for the
default pair.

#### Window glass

The root still paints `rgba(p.glass)`. Light and pastel templates raise the
alpha to 0.85 so cards stay readable over a dark wallpaper, while dark keeps
today's 0.38. The compositor blur setup in `main.rs` does not change.

#### Viewer and photo overlays

The full-screen viewer follows the main colour, including light and pastel
mains:

- The stage (letterbox) uses `p.bg`; the morph backdrop uses the same. The
  image's average-colour wash stays image-derived and keeps its alpha.
- The top bar over the photo uses `with_alpha(p.panel_header, 0x99)` with
  `p.line_soft` as its bottom border, so it is dark-on-light in light themes
  and light-on-dark in dark themes. Labels use `p.muted`, actions use the
  existing `focusable_text_button` palette tokens.
- The year-rail marker over the gallery uses `p.panel_header` with `p.text`
  and a `p.border_strong` ring; it no longer assumes a dark pill.
- Modal scrims (`import`, manual compression, dialogs) use `p.scrim`. A black
  scrim is still correct in light themes; only its strength follows the
  family (0.80 dark, 0.35 light/pastel; the dark value keeps the darker
  backdrops added with the notification work).

### 5. Settings UI

A new card at the top of the Settings screen, above "ライブラリ":

```
┌ テーマ ────────────────────────────────────────────┐
│ メインカラー（背景・パネル）                        │
│  ●  ●  ●  ●  ●  ●                                  │
│  ●  ●  ●  ●  ●  ●                                  │
│  ●  ●  ●  ●  ●  ●                                  │
│ サブカラー（アクセント）                            │
│  ●  ●  ●  ●  ●  ●                                  │
│  ●  ●  ●  ●  ●  ●                                  │
│  ●  ●  ●  ●  ●  ●                                  │
│ ※サブカラーは背景とのコントラストに合わせて         │
│   明度を自動調整します                              │
└─────────────────────────────────────────────────────┘
```

- Each option is a 30 px circle (24 px fill + 2 px ring) with no inline name
  and no family headings. Hovering shows a tooltip with the colour name.
  Selected entries get a 2 px `accent_border` ring and a check mark whose
  colour is `readable_on(swatch)`.
- Main preview = the retinted `panel_2`, for every family. It is light enough
  in dark themes and tinted enough in light/pastel themes to distinguish the
  six colours; `panel_card` would render all light swatches as pure white and
  all dark swatches as near-black. Accent preview = the entry's raw base
  colour.
- Layout: left-aligned rows of six circles with an 8 px gap. The rows do not
  stretch to the card width; the catalog order (dark, light, pastel) is kept
  without headings. The section header shows the selected colour name
  (`メインカラー（背景・パネル） · チャコール`).
- Clicking applies immediately and saves; there is no Apply button. A save
  failure restores the previous selection and reports through the existing
  message/toast path.
- The card includes the contrast note; no per-swatch warnings.
- Keyboard: chips are regular clickable elements like the max-FPS chips in
  the same screen. No new focus system.

New setters on `App`:

```rust
pub(crate) fn set_main_theme(&mut self, id: &str, cx: &mut Context<Self>);
pub(crate) fn set_accent_theme(&mut self, id: &str, cx: &mut Context<Self>);
```

Both follow this order: resolve → assign `self.palette` → save → on error
revert settings + palette → apply component theme → `cx.notify()`.

### 6. Persistence

Add to `AppSettings`:

```rust
#[serde(default = "default_ui_theme_main")]
pub ui_theme_main: String,      // "charcoal"

#[serde(default = "default_ui_theme_accent")]
pub ui_theme_accent: String,    // "sky"
```

- Missing fields (old databases) default to the current look.
- Unknown or empty IDs fall back to the default entry in the UI catalog; they
  are never treated as an error.
- IDs are stable strings; adding a colour never renumbers an existing one.
- Core only stores the IDs; the catalog and validation live in the UI crate.

### 7. Removing hardcoded colours

Every `rgb(0x...)` / `rgba(0x...)` outside `palette.rs` is replaced by a
`Palette` field, except:

- fully transparent `rgba(0x00000000)`;
- image bytes and `average_bgra` inputs (not a UI colour);
- test fixtures that do not render.

Mapping rules for the ~150 literals:

| Existing pattern | Replacement |
| --- | --- |
| `rgba(0xffffff08..1b)` on borders/rows | `p.line_soft` or `p.highlight` (choose by role: border vs inset top light) |
| `rgba(0xffffffXX)` on hover fills | `p.panel_2` / `p.panel_inner` (alpha no longer needed) |
| `rgba(0x000000XX)` shadows | `p.shadow` |
| `rgba(0x000000XX)` / `0x05070aXX` modal backdrops | `p.scrim` |
| `0x05070a` viewer stage / morph backdrop | `p.bg` |
| `rgba(0x05070a99)` viewer top bar | `with_alpha(p.panel_header, 0x99)` |
| `0x05070af2` year-rail marker | `p.panel_header` (+ `p.text`, `p.border_strong` ring) |
| `rgba(0x66b3ffXX)`, `0xdceeff`, `0x1a2f47` | `p.accent`, `p.accent_soft`, `p.accent_fill`, or `with_alpha(p.accent, a)` |
| `0xfb7185`, `0x2a1414`, `0xffd9df`, red alphas | `p.red`, `p.red_soft`, `p.red_border`, `p.text` |
| `0x5eead4` / `0xfbbf24` families | `p.green*` / `p.amber*` |
| ad-hoc neutral surfaces (`0x0f1725`, `0x101826`, …) | nearest of `panel_header`, `panel_card`, `panel_2`, `panel_inner` |
| gradients | two existing palette fields |
| `scene_color` table | `p.scene_color(world)` |

A source-scan test (below) enforces this after the sweep.

### 8. Startup sequence

1. `gpui_kit::init(cx)` (still forces light internally).
2. `App::new` loads `AppSettings`, resolves `self.palette`, and calls
   `apply_component_theme`.
3. The first frame paints the saved theme. No dark flash.
4. The explicit `ThemeMode::Dark` block in `main.rs` is removed; the
   transparent-background fix moves into `apply_component_theme`.

## Edge cases

- **White accent + light main**: swatch stays white; text accent is darkened
  to pass 4.5:1; primary fills fall back to the adjusted accent when the raw
  fill would disappear into the card.
- **Pastel main + white accent**: same rule; the soft fill keeps a tint of the
  adjusted accent.
- **Unknown/invalid IDs**: default entry; the saved string is replaced on the
  next save.
- **Save failure**: palette and settings revert together; the message says the
  setting could not be saved.
- **Existing databases**: missing fields default to `charcoal` + `sky`.
- **Notifications branch**: this branch already contains `feat/ui-notifications`
  (`33276fd` and earlier). Build on it; do not rebase onto `main` alone.
- **Multiple windows** (not used today): the palette is per-`App`, and
  `AppSettings` is shared through SQLite, so a future second window reads the
  same IDs at startup.

## Tests

Unit tests in `palette.rs`:

- HSL ↔ RGB round-trips for representative colours.
- `contrast_ratio(0x000000, 0xffffff) == 21.0` (±0.01); known pairs.
- `ensure_contrast` preserves hue (within 2°) and reaches the requested ratio.
- Catalog: 18 entries per axis, 6 per family, unique IDs, labels non-empty.
- `resolve("unknown", ...)` and `resolve("", ...)` fall back to defaults.
- Default pair: `accent` resolves to `0x66b3ff`; `charcoal` surfaces equal the
  current constants; `accent_soft` is within 4 RGB steps of `0x1a2f47`.
- All 324 combinations:
  - `text` / `panel_card` ≥ 7.0
  - `muted` / `panel_card` ≥ 4.5
  - `muted_2` / `panel_card` ≥ 3.0
  - `accent` / `panel_card`, `panel_inner`, `panel_2`, `accent_soft`,
    `accent_soft_hover` ≥ 4.5
  - `red`, `amber`, `green` / their own soft fills ≥ 4.5
  - `border` / `panel_card` ≥ 1.2; `border_strong` ≥ 1.8
  - `on_accent` / `accent_fill` ≥ 4.5
- `scene_color` returns a family-appropriate lightness (dark < 0.25, light >
  0.80, pastel > 0.75).

Settings tests (`crates/core`):

- `ui_theme_main` / `ui_theme_accent` round-trip.
- Old JSON without the fields loads as `charcoal` / `sky`.
- An unknown ID survives load (UI falls back) without resetting other fields.

Source guard (UI test):

- Walk `crates/ui/src/**/*.rs`; fail on `rgb(0x` / `rgba(0x` outside
  `palette.rs`, excluding fully transparent `0x00000000` and `#[cfg(test)]`
  fixtures. If walking files is too brittle, use `include_str!` for the
  affected modules. This is the regression guard for light mode.

Manual verification (documented in the PR):

- Default pair looks identical to v0.8.0 except the `gpui-component` primary
  button now takes the accent.
- One light and one pastel main, each with a dark, light, and pastel accent:
  read the settings card, library, viewer (stage, top bar, details, morph),
  year rail, dialogs, and notifications.
- All four widget classes (button, input, switch, scrollbar) follow the theme.
- Restart keeps the selection.

## Impact

- New: `crates/ui/src/ui/palette.rs` (~600 lines with tests).
- `crates/core/src/settings.rs`: two fields, defaults, tests.
- `crates/ui/src/ui/theme.rs`: colour constants removed, re-exports the
  palette, `scene_color` moves.
- `crates/ui/src/app.rs`, `main.rs`: palette field, startup apply.
- `crates/ui/src/ui/settings.rs`: theme card and setters.
- `crates/ui/src/ui/components.rs` and every screen module: constant →
  palette field, literal → token (mechanical, ~25 files).
- `crates/ui/src/update_view.rs`, `ui/year_rail.rs`, `state/year_rail.rs`:
  palette parameters / placeholder tint.

## Risks

1. **The literal sweep is the real work.** A missed white/blue literal breaks
   one family (usually light). The source-scan test plus one manual pass over
   a light and a pastel main is the mitigation.
2. **Component theme mapping.** Buttons read `tokens`, inputs read `colors`.
   Both are set; the checklist covers all four widget classes.
3. **Default drift.** Dark + sky keeps the app's own surfaces byte-identical,
   but `gpui-component` widgets move from the component defaults to palette
   tokens: input borders/fills, primary buttons, and selection tints take the
   accent. Capture a before/after screenshot in the PR.
4. **Stacked branch.** `feat/ui-notifications` is the base, so a bug in the
   notifications work is part of the same PR. Test both features before
   opening it.
5. **Pastel saturation.** Hue rotation at high saturation can make surfaces
   look neon; the templates use lower saturation and the contrast guards keep
   text readable. Adjust the pastel template values if the visual pass finds
   one too strong.

## Decisions made in this document

- Six colours per family (18 per axis, 324 combinations) as the interpretation
  of "many".
- Families are dark / light / pastel, both for main and accent.
- Defaults preserve the current dark + blue look.
- Accent contrast is fixed by adjusting lightness only; swatches show the raw
  colour.
- Status colours keep their hue and only adjust lightness.
- Light and pastel mains are real light themes (dark text, light cards).
- The full-screen viewer follows the main colour too; its photo overlays
  (top bar, year rail) are derived from the same palette.
- The theme card sits at the top of Settings; changes apply instantly.
- No free colour picker, no OS-follow, no import/export in this change.
- The branch is stacked on `feat/ui-notifications`; the final PR is one PR
  against `main` containing the notifications commits, this design, the plan,
  and the implementation (user instruction).
