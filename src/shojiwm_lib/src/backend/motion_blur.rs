//! `<ManagedWindow motionBlur>`: a window's streak, drawn as one pass over
//! the pixels it and the windows moving with it cover, once they and
//! everything under them are in the framebuffer. The window itself is drawn as at rest, so a streak that
//! comes and goes changes nothing else about its frame.

use std::collections::HashMap;

use smithay::{
    backend::renderer::gles::GlesRenderer,
    desktop::Window,
    utils::{Logical, Rectangle, Scale},
};

use crate::{
    backend::shader_effect::{
        ShaderEffectElementState, ShaderEffectError, StableBackdropFramebufferElement,
        framebuffer_backdrop_element_for_output_rects,
    },
    ssd::{
        CompiledEffect, DirectionalBlur, EffectAlphaMode, EffectInput, EffectInvalidationPolicy,
        EffectStage, LogicalRect,
    },
};

/// The most taps one pass takes; `directional_blur.frag` loops to it.
pub const MAX_TAPS: u32 = 16;

/// A streak shorter than this many logical pixels is not drawn: the pass
/// would give back the pixels it read.
pub const SHORTEST: f64 = 0.5;

/// The longest streak drawn, in logical pixels. A config asks for far less;
/// this bounds the capture a wrong value could ask for.
pub const LONGEST: f64 = 96.0;

/// The taps for a streak `length` physical pixels long: a pixel apart at
/// most while there are taps to spend, and never one alone, so the blur
/// grows from nothing as the streak does.
pub fn taps(length: f32) -> u32 {
    (length.max(0.0).ceil() as u32 + 1).clamp(2, MAX_TAPS)
}

/// The step the pass's margin round the window grows in, in logical
/// pixels. A streak changes length every frame of a move; in steps the
/// capture keeps its size through it and its texture is not made anew.
pub const REACH_STEP: i32 = 8;

/// What a streak asks of the frame: the stage, and how far past the window
/// the pass goes on every side, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    pub blur: DirectionalBlur,
    pub reach: i32,
}

/// `None` for a streak too short to show.
pub fn plan(streak: [f64; 2]) -> Option<Plan> {
    let [x, y] = streak;
    let length = x.hypot(y);
    if !length.is_finite() || length < SHORTEST {
        return None;
    }
    let keep = (LONGEST / length).min(1.0);
    let (x, y) = (x * keep, y * keep);
    Some(Plan {
        blur: DirectionalBlur {
            x: x as f32,
            y: y as f32,
        },
        // Half the streak lands on either side of an edge.
        reach: ((length * keep / 2.0 / f64::from(REACH_STEP)).ceil() as i32) * REACH_STEP,
    })
}

impl Plan {
    /// The rect the pass redraws for a window drawn in `window`.
    pub fn rect(&self, window: LogicalRect) -> LogicalRect {
        LogicalRect::new(
            window.x - self.reach,
            window.y - self.reach,
            window.width + 2 * self.reach,
            window.height + 2 * self.reach,
        )
    }

    fn effect(&self) -> CompiledEffect {
        CompiledEffect {
            input: EffectInput::Backdrop,
            // The taps of the rect's outermost pixels read this far past it.
            capture_padding: self.reach + 1,
            invalidate: EffectInvalidationPolicy::Always,
            pipeline: vec![EffectStage::DirectionalBlur(self.blur)],
            alpha: EffectAlphaMode::Opaque,
        }
    }
}

/// The element that draws `streak` over the windows drawn in `windows`, to
/// go in front of everything of the topmost of them.
pub fn element(
    renderer: &mut GlesRenderer,
    state: &mut ShaderEffectElementState,
    streak: [f64; 2],
    windows: &[LogicalRect],
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Result<Option<StableBackdropFramebufferElement>, ShaderEffectError> {
    let Some(plan) = plan(streak) else {
        return Ok(None);
    };
    let rects: Vec<_> = windows
        .iter()
        .filter(|window| window.width > 0 && window.height > 0)
        .map(|window| plan.rect(*window))
        .filter(|rect| {
            Rectangle::new((rect.x, rect.y).into(), (rect.width, rect.height).into())
                .overlaps(output_geo)
        })
        .collect();
    framebuffer_backdrop_element_for_output_rects(
        renderer,
        state,
        &rects,
        plan.effect(),
        output_geo,
        scale,
        1.0,
    )
}

/// Streaks a sixteenth of a pixel apart are one streak: the windows of a
/// tape or a workspace move as one and are blurred in one pass.
fn key(streak: [f64; 2]) -> [i64; 2] {
    streak.map(|part| (part * 16.0).round() as i64)
}

/// `window`'s decoration if it is drawn with `streak`.
fn moving<'a>(
    decorations: &'a HashMap<Window, crate::ssd::WindowDecorationState>,
    window: &Window,
    streak: [f64; 2],
) -> Option<&'a crate::ssd::WindowDecorationState> {
    decorations.get(window).filter(|decoration| {
        let state = &decoration.managed_window;
        state.visible && plan(state.motion_blur).is_some() && key(state.motion_blur) == key(streak)
    })
}

/// The streak element to draw in front of `window` as `output` draws it.
/// Windows with the same streak share one, held by the topmost of them in
/// `windows_top_to_bottom`: each pass is a capture and a draw of its own,
/// and their cost is in their number, not their size.
pub fn element_for_window(
    renderer: &mut GlesRenderer,
    decorations: &mut HashMap<Window, crate::ssd::WindowDecorationState>,
    windows_top_to_bottom: &[Window],
    window: &Window,
    output_name: &str,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Option<StableBackdropFramebufferElement> {
    let cache = format!("motion-blur@{output_name}");
    let streak = decorations.get(window)?.managed_window.motion_blur;
    let topmost = windows_top_to_bottom
        .iter()
        .find(|other| moving(decorations, other, streak).is_some());
    if plan(streak).is_none() || topmost != Some(window) {
        decorations.get_mut(window)?.shader_cache.remove(&cache);
        return None;
    }
    let rects: Vec<_> = windows_top_to_bottom
        .iter()
        .filter_map(|other| moving(decorations, other, streak))
        .map(|decoration| {
            crate::backend::visual::transformed_root_rect(
                decoration.layout.root.rect,
                decoration.visual_transform,
            )
        })
        .collect();
    let state = decorations
        .get_mut(window)?
        .shader_cache
        .entry(cache)
        .or_default();
    element(renderer, state, streak, &rects, output_geo, scale)
        .inspect_err(|error| tracing::warn!(?error, "failed to build a window's motion blur"))
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_at_rest_or_barely_moving_gets_no_pass() {
        assert_eq!(plan([0.0, 0.0]), None);
        assert_eq!(plan([0.3, -0.3]), None);
        assert_eq!(plan([f64::NAN, 0.0]), None);
        assert!(plan([0.5, 0.0]).is_some());
    }

    #[test]
    fn the_pass_reaches_past_the_window_by_a_margin_a_move_keeps() {
        let window = LogicalRect::new(100, 50, 800, 600);
        let rect = plan([-9.0, 0.0]).unwrap().rect(window);
        assert_eq!(rect, LogicalRect::new(92, 42, 816, 616));
        // The same through a move: as the streak grows, shrinks and turns.
        for streak in [[0.6, 0.0], [0.0, -12.0], [8.0, 8.0], [16.0, 0.0]] {
            assert_eq!(plan(streak).unwrap().rect(window), rect, "{streak:?}");
        }
        assert_eq!(plan([17.0, 0.0]).unwrap().reach, 16);
        let plan = plan([-9.0, 0.0]).unwrap();
        // Its taps read no further than the capture goes.
        assert!(plan.effect().capture_padding as f32 >= plan.blur.length() / 2.0);
    }

    #[test]
    fn windows_that_move_as_one_share_a_streak() {
        assert_eq!(key([9.25, 0.0]), key([9.26, 0.0]));
        assert_ne!(key([9.25, 0.0]), key([9.5, 0.0]));
        assert_ne!(key([9.25, 0.0]), key([0.0, 9.25]));
        assert_ne!(key([9.25, 0.0]), key([-9.25, 0.0]));
    }

    #[test]
    fn a_streak_keeps_its_direction_when_it_is_cut_to_the_longest() {
        let plan = plan([300.0, -400.0]).unwrap();
        assert!((f64::from(plan.blur.length()) - LONGEST).abs() < 1e-3);
        assert!((plan.blur.x / plan.blur.y + 0.75).abs() < 1e-6);
    }

    #[test]
    fn taps_are_a_pixel_apart_until_they_run_out() {
        assert_eq!(taps(0.0), 2);
        assert_eq!(taps(0.4), 2);
        assert_eq!(taps(4.0), 5);
        assert_eq!(taps(9.5), 11);
        assert_eq!(taps(15.0), MAX_TAPS);
        assert_eq!(taps(60.0), MAX_TAPS);
        for length in [0.5_f32, 3.0, 9.5, 14.9] {
            assert!(length / taps(length) as f32 <= 1.0);
        }
    }
}
