//! Viewer open/close state and the morph timelines used by Quick Look.
//!
//! Screen painting and `App` methods stay in the UI layer; this module owns
//! the state values and the pure timeline helpers they share.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{Bounds, Pixels, RenderImage};

use crate::ui::theme::{
    MOTION_MORPH, eased_progress, lerp_bounds, viewer_image_box_with_details_width,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewerOpenSource {
    Card,
    Inspector,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MorphImageKind {
    Thumbnail,
    Preview,
}

/// Chooses the old/new image kinds and the corner-radius sweep for one morph.
pub(crate) fn morph_image_plan(
    closing: bool,
    source: ViewerOpenSource,
    has_thumbnail: bool,
    has_preview: bool,
) -> (Option<MorphImageKind>, Option<MorphImageKind>, f32, f32) {
    let viewer = if has_preview {
        Some(MorphImageKind::Preview)
    } else if has_thumbnail {
        Some(MorphImageKind::Thumbnail)
    } else {
        None
    };
    let Some(viewer) = viewer else {
        return (None, None, 10.0, 10.0);
    };
    let card = if has_thumbnail {
        Some(MorphImageKind::Thumbnail)
    } else if has_preview {
        Some(MorphImageKind::Preview)
    } else {
        None
    };
    let inspector = if has_preview {
        Some(MorphImageKind::Preview)
    } else if has_thumbnail {
        Some(MorphImageKind::Thumbnail)
    } else {
        None
    };
    let inspector_source = source == ViewerOpenSource::Inspector;
    if closing {
        (
            Some(viewer),
            if inspector_source { inspector } else { card },
            10.0,
            if inspector_source { 11.0 } else { 12.0 },
        )
    } else {
        (
            if inspector_source { inspector } else { card },
            Some(viewer),
            if inspector_source { 11.0 } else { 12.0 },
            10.0,
        )
    }
}

pub(crate) struct ViewerMorph {
    pub(crate) epoch: u64,
    pub(crate) started_at: Instant,
    pub(crate) from: Bounds<Pixels>,
    pub(crate) to: Bounds<Pixels>,
    pub(crate) thumbnail: Option<Arc<RenderImage>>,
    pub(crate) preview: Option<Arc<RenderImage>>,
    pub(crate) color: u32,
    pub(crate) source: ViewerOpenSource,
    pub(crate) closing: bool,
    pub(crate) fade_only: bool,
    pub(crate) backdrop_from: f32,
}

impl ViewerMorph {
    pub(crate) fn current_bounds(&self, now: Instant) -> Bounds<Pixels> {
        if self.fade_only {
            return self.from;
        }
        morph_bounds_at(self.from, self.to, self.started_at, now)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ViewerBoxState {
    pub(crate) epoch: u64,
    pub(crate) started_at: Instant,
    pub(crate) from_size: (f32, f32),
    pub(crate) to_aspect: f32,
    pub(crate) to_details_width: f32,
    pub(crate) duration: Duration,
}

impl ViewerBoxState {
    pub(crate) fn size_at(&self, t: f32, viewport: gpui_kit::Size<Pixels>) -> (f32, f32) {
        let t = t.clamp(0.0, 1.0);
        let to = viewer_image_box_with_details_width(
            viewport.width.into(),
            viewport.height.into(),
            self.to_details_width,
            self.to_aspect,
        );
        (
            self.from_size.0 + (to.0 - self.from_size.0) * t,
            self.from_size.1 + (to.1 - self.from_size.1) * t,
        )
    }

    pub(crate) fn current_size(&self, viewport: gpui_kit::Size<Pixels>) -> (f32, f32) {
        let t = eased_progress(self.started_at, self.duration, Instant::now());
        self.size_at(t, viewport)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ViewerPanelState {
    pub(crate) epoch: u64,
    pub(crate) started_at: Instant,
    pub(crate) from_width: f32,
    pub(crate) to_width: f32,
    pub(crate) duration: Duration,
}

impl ViewerPanelState {
    pub(crate) fn width_at(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        self.from_width + (self.to_width - self.from_width) * t
    }

    pub(crate) fn current_width(&self) -> f32 {
        self.current_width_at(Instant::now())
    }

    fn current_width_at(&self, now: Instant) -> f32 {
        self.width_at(eased_progress(self.started_at, self.duration, now))
    }
}

pub(crate) fn morph_bounds_at(
    from: Bounds<Pixels>,
    to: Bounds<Pixels>,
    started_at: Instant,
    now: Instant,
) -> Bounds<Pixels> {
    lerp_bounds(from, to, eased_progress(started_at, MOTION_MORPH, now))
}

pub(crate) fn current_tagged_bounds(
    entry: Option<(u64, Bounds<Pixels>)>,
    current_epoch: u64,
) -> Option<Bounds<Pixels>> {
    entry
        .filter(|(epoch, _)| *epoch == current_epoch)
        .map(|(_, bounds)| bounds)
}

pub(crate) fn preferred_bounds(
    prefer_card: bool,
    card: Option<Bounds<Pixels>>,
    inspector: Option<Bounds<Pixels>>,
) -> Option<Bounds<Pixels>> {
    if prefer_card {
        card.or(inspector)
    } else {
        inspector.or(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{MOTION_BASE, MOTION_SLOW, VIEWER_DETAILS_WIDTH};
    use gpui_kit::{point, px, size};

    #[test]
    fn morph_bounds_at_hits_both_ends() {
        let start = Instant::now();
        let from = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
        let to = Bounds::new(point(px(100.), px(200.)), size(px(300.), px(400.)));
        assert_eq!(morph_bounds_at(from, to, start, start), from);
        assert_eq!(morph_bounds_at(from, to, start, start + MOTION_MORPH), to);
    }

    #[test]
    fn viewer_morph_current_bounds_follows_the_timeline() {
        let from = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(80.)));
        let to = Bounds::new(point(px(200.), px(100.)), size(px(400.), px(300.)));
        let start = Instant::now();
        let morph = ViewerMorph {
            epoch: 1,
            started_at: start,
            from,
            to,
            thumbnail: None,
            preview: None,
            color: 0,
            source: ViewerOpenSource::Card,
            closing: false,
            fade_only: false,
            backdrop_from: 1.0,
        };
        assert_eq!(morph.current_bounds(start), from);
        assert_eq!(morph.current_bounds(start + MOTION_MORPH), to);
        let middle = morph.current_bounds(start + MOTION_MORPH / 2);
        assert!(middle.origin.x > from.origin.x && middle.origin.x < to.origin.x);
    }

    #[test]
    fn viewer_box_state_interpolates_from_the_visible_size() {
        let viewport = size(px(1200.), px(800.));
        let from_size = (800.0, 500.0);
        let state = ViewerBoxState {
            epoch: 1,
            started_at: Instant::now(),
            from_size,
            to_aspect: 2.0,
            to_details_width: 0.0,
            duration: MOTION_BASE,
        };
        let (w0, h0) = state.size_at(0.0, viewport);
        let (w1, h1) = state.size_at(1.0, viewport);
        let (wm, hm) = state.size_at(0.5, viewport);
        assert_eq!((w0, h0), from_size);
        assert_eq!(w1, 1152.0);
        assert_eq!(h1, 576.0);
        assert!(wm > w0 && wm < w1);
        assert!(hm > h0 && hm < h1);
    }

    #[test]
    fn viewer_box_state_hits_both_details_ends() {
        let viewport = size(px(1200.), px(800.));
        let state = ViewerBoxState {
            epoch: 2,
            started_at: Instant::now(),
            from_size: (900.0, 560.0),
            to_aspect: 16.0 / 10.0,
            to_details_width: VIEWER_DETAILS_WIDTH,
            duration: MOTION_SLOW,
        };
        let (closed, _) = state.size_at(0.0, viewport);
        let (open, _) = state.size_at(1.0, viewport);
        assert!(closed > open);
    }

    #[test]
    fn viewer_panel_state_retargets_from_the_current_width() {
        let start = Instant::now();
        let state = ViewerPanelState {
            epoch: 1,
            started_at: start,
            from_width: 120.0,
            to_width: 0.0,
            duration: MOTION_SLOW,
        };
        assert_eq!(state.width_at(0.0), 120.0);
        assert_eq!(state.width_at(1.0), 0.0);
        let middle = state.width_at(0.5);
        assert!(middle > 0.0 && middle < 120.0);
        assert_eq!(state.current_width_at(start + MOTION_SLOW), 0.0);
    }

    #[test]
    fn morph_image_plan_covers_card_and_inspector_paths() {
        use MorphImageKind::*;
        assert_eq!(
            morph_image_plan(false, ViewerOpenSource::Card, true, true),
            (Some(Thumbnail), Some(Preview), 12.0, 10.0)
        );
        assert_eq!(
            morph_image_plan(false, ViewerOpenSource::Inspector, true, true),
            (Some(Preview), Some(Preview), 11.0, 10.0)
        );
        assert_eq!(
            morph_image_plan(true, ViewerOpenSource::Card, true, true),
            (Some(Preview), Some(Thumbnail), 10.0, 12.0)
        );
        assert_eq!(
            morph_image_plan(true, ViewerOpenSource::Inspector, true, true),
            (Some(Preview), Some(Preview), 10.0, 11.0)
        );
        assert_eq!(
            morph_image_plan(false, ViewerOpenSource::Card, false, true),
            (Some(Preview), Some(Preview), 12.0, 10.0)
        );
        assert_eq!(
            morph_image_plan(true, ViewerOpenSource::Inspector, true, false),
            (Some(Thumbnail), Some(Thumbnail), 10.0, 11.0)
        );
        assert_eq!(
            morph_image_plan(false, ViewerOpenSource::Card, false, false),
            (None, None, 10.0, 10.0)
        );
    }

    #[test]
    fn current_tagged_bounds_rejects_stale_frames() {
        let bounds = Bounds::new(point(px(1.), px(2.)), size(px(3.), px(4.)));
        assert_eq!(current_tagged_bounds(Some((7, bounds)), 7), Some(bounds));
        assert_eq!(current_tagged_bounds(Some((7, bounds)), 8), None);
        assert_eq!(current_tagged_bounds(None, 7), None);
    }

    #[test]
    fn preferred_bounds_falls_back_to_the_other_source() {
        let card = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
        let inspector = Bounds::new(point(px(20.), px(20.)), size(px(30.), px(30.)));
        assert_eq!(
            preferred_bounds(true, Some(card), Some(inspector)),
            Some(card)
        );
        assert_eq!(
            preferred_bounds(true, None, Some(inspector)),
            Some(inspector)
        );
        assert_eq!(
            preferred_bounds(false, Some(card), Some(inspector)),
            Some(inspector)
        );
        assert_eq!(preferred_bounds(false, Some(card), None), Some(card));
    }

    #[test]
    fn closing_prefers_the_original_source() {
        let card = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(10.)));
        let inspector = Bounds::new(point(px(20.), px(20.)), size(px(30.), px(30.)));
        assert_eq!(
            preferred_bounds(true, Some(card), Some(inspector)),
            Some(card)
        );
        assert_eq!(
            preferred_bounds(false, Some(card), Some(inspector)),
            Some(inspector)
        );
    }
}
