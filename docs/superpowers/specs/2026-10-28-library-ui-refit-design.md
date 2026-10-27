# Library UI Refit — Approved Design

Date: 2026-10-28
Status: Approved by the user in chat.

## Intent

Fix the library/search layout and interaction defects, then refit the header,
sidebar, settings, compression, and dashboard around the approved behavior.
Preserve existing functionality unless the user explicitly asked for removal.

## Approved decisions

### Library / Search

- Remove the per-preset column cap. Column count is derived from the available
  width and the preset minimum card width: Small 140, Medium 190, Large 220,
  ExtraLarge 280.
- Increase `GRID_GAP` from 9 px to 14 px.
- When world/date/size are all hidden, the image fills the card and all four
  corners use the card radius (`ObjectFit::Cover`).
- Align the inspector card's right edge with the header card's right edge and
  remove the extra translucent strip.
- Keep the inspector's fullscreen action. Its morph must originate from the
  normal image card's image rect, not the inspector preview.
- Optimize the existing morph first; simplify to fade + scale only if it still
  stutters.
- The gallery month header follows the first fully visible row, not the first
  entry in the index.
- Remove the library filter button and the "並び: 新しい順" label. Keep the
  Search tab's left filter panel. Remove the Search tab's sort label too.
- Treat pointer movement up to 4 px as a click.

### Compression

- Switching profiles must not blank the whole card.
- Profile editor fields are grouped with section headings and dividers; the
  layout uses 1 or 2 columns depending on width.
- Output directory is a text field in the profile editor. Relative names create
  a subfolder under each source image's directory. Absolute paths are also
  allowed. Provide a browse button and a clear relative/absolute indication.
  Migrate existing absolute values by keeping the path absolute; do not lose
  data.
- Dashboard "未実行" shows the total byte size of all image files.
- Compression actions are split into: specified folder, profile-specified
  folder, clipboard. The compression modal hides the output-destination menu.
- If a profile has the system clipboard flag, the clipboard action skips the
  modal and compresses immediately.
- Remove compression ribbon menus except "待機を圧縮".

### Settings / Sidebar / Header

- Remove the settings-internal sidebar; settings use one column.
- Sidebar: remove the top icon/title and the bottom "監視中" label. Put the
  expand/collapse control where the icon/title was.
- Collapsed sidebar hover descriptions appear immediately to the right of the
  hovered item, not at the app's right edge.
- Collapsed sidebar bottom shows compact "未実行 N件" and "N枚 / X GB" lines.
- Header/ribbon is collapsed by default and expands on click, with an animation.
- Remove the Home/Photos/Compression tab selection. When expanded, show all
  ribbon menus with group headings.
- Remove header items: open folder, rebuild index, favorites, fullscreen, tags,
  notes. Keep the corresponding detail-panel actions.
- Move the card-size setting to the header as a Small/Medium/Large/ExtraLarge
  segmented control.

### System

- Initial profiles (new databases only):
  1. Auto: "無劣化webp", lossless, no resize, output `compressed/`, no suffix.
  2. Manual: "discord用webp", lossless, downscale only above 2K
     (2560x1440), output `discord/`, suffix `_discord`, clipboard flag.
  3. Manual: "高画質webp", quality 90, no resize, output `webp/`, no suffix.
- The clipboard flag is a per-profile toggle, at most one profile system-wide,
  shown as a badge in the profile list.
- Audit GPUI usage for anti-patterns and unnecessary overhead without removing
  features.
- Fix the tab-switch background flash.
- Reuse the profile-delete confirmation modal for image deletion, bulk delete,
  and update installation.

## Implementation order

1. Library / Search
2. Header / Sidebar / Settings
3. Compression
4. Performance / Modal
