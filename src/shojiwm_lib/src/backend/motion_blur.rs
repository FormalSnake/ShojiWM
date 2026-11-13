//! A managed window's `motion_blur`: its streak, drawn as one pass over the
//! pixels it and the windows moving with it cover, once they and everything
//! under them are in the framebuffer. The window itself is drawn as at
//! rest, so a streak that comes and goes changes nothing else about its
//! frame.
//!
//! The pass is a copy of the output's pixels under the windows into a
//! texture, then one draw per window that reads the copy along the streak.
//! The texture is as large as the output and goes back to a spare pile when
//! the move ends, so no move makes one after the first.

use std::{
    cell::RefCell,
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Frame as _, FrameContext as _, Offscreen, Texture,
            element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
            gles::{
                GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
                UniformName, UniformType, ffi,
            },
            utils::{CommitCounter, OpaqueRegions},
        },
    },
    desktop::Window,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, user_data::UserDataMap},
};

use crate::{
    backend::shader_effect::{
        WindowEffectElementState, begin_gpu_timing_frame_span, end_gpu_timing_frame_span,
        ensure_blur_scratch_fbo, relative_intersections, with_gpu_timing_gl_span,
    },
    ssd::{DirectionalBlur, LogicalRect, WindowDecorationState},
};

/// The most taps one pass takes; the shaders loop to it.
pub const MAX_TAPS: u32 = 16;

/// A streak shorter than this many logical pixels is not drawn: the pass
/// would give back the pixels it read.
pub const SHORTEST: f64 = 0.5;

/// The longest streak drawn, in logical pixels. A config asks for far less;
/// this bounds what a wrong value could ask for.
pub const LONGEST: f64 = 96.0;

/// The step the pass's margin round the window grows in, in logical
/// pixels, so the pass covers the same pixels through a move while the
/// streak changes length every frame of it.
pub const REACH_STEP: i32 = 8;

/// The spare textures kept: one for each output mode in use and one over.
const SPARES: usize = 4;

/// The taps for a streak `length` physical pixels long: a pixel apart at
/// most while there are taps to spend, and never one alone, so the blur
/// grows from nothing as the streak does.
pub fn taps(length: f32) -> u32 {
    (length.max(0.0).ceil() as u32 + 1).clamp(2, MAX_TAPS)
}

/// What a streak asks of the frame: the blur, and how far past the window
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

    /// How far past the pass the copy goes, in logical pixels: the taps of
    /// its outermost pixels read this far.
    pub fn padding(&self) -> i32 {
        self.reach + 1
    }
}

/// Streaks a sixteenth of a pixel apart are one streak: the windows of a
/// tape or a workspace move as one.
fn key(streak: [f64; 2]) -> [i64; 2] {
    streak.map(|part| (part * 16.0).round() as i64)
}

/// The windows a pass in front of `stack[index]` covers. `stack` is what
/// an output draws, top to bottom: the key of each window's streak, `None`
/// for one that is still. Windows next to each other in the stack with one
/// streak share the pass in front of the topmost of them. A still window
/// between two that move ends the run, or the pass of those above it would
/// blur it where it lies over those below.
fn run(stack: &[Option<[i64; 2]>], index: usize) -> Option<std::ops::Range<usize>> {
    let streak = stack.get(index).copied().flatten()?;
    if index > 0 && stack[index - 1] == Some(streak) {
        return None;
    }
    let length = stack[index..]
        .iter()
        .take_while(|other| **other == Some(streak))
        .count();
    Some(index..index + length)
}

/// Capture textures no pass holds, kept with the GL context.
#[derive(Debug, Default)]
struct Spare {
    textures: Vec<GlesTexture>,
    /// How many were ever made.
    made: usize,
}

type Spares = Arc<Mutex<Spare>>;

fn spares(renderer: &mut GlesRenderer) -> Spares {
    let data = renderer.egl_context().user_data();
    data.insert_if_missing(Spares::default);
    data.get::<Spares>()
        .expect("the spare textures were just put there")
        .clone()
}

fn take(
    renderer: &mut GlesRenderer,
    spares: &Spares,
    size: Size<i32, Buffer>,
) -> Result<GlesTexture, GlesError> {
    let mut spare = spares
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(at) = spare
        .textures
        .iter()
        .position(|texture| texture.size() == size)
    {
        return Ok(spare.textures.swap_remove(at));
    }
    spare.made += 1;
    let texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, size)?;
    // A new texture repeats: a tap past the output's edge would read its
    // far side.
    let id = texture.tex_id();
    renderer.with_context(|gl| unsafe {
        gl.BindTexture(ffi::TEXTURE_2D, id);
        for side in [ffi::TEXTURE_WRAP_S, ffi::TEXTURE_WRAP_T] {
            gl.TexParameteri(ffi::TEXTURE_2D, side, ffi::CLAMP_TO_EDGE as i32);
        }
        gl.BindTexture(ffi::TEXTURE_2D, 0);
    })?;
    Ok(texture)
}

/// What a pass holds between its copy and its draw, and from one frame of
/// a move to the next: in the damage tracker's cache for its element, so
/// it goes when the element does.
#[derive(Debug)]
struct Held {
    /// The output's pixels, laid out as its framebuffer has them.
    texture: Option<GlesTexture>,
    spares: Spares,
}

impl Held {
    fn fit(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Buffer>,
    ) -> Result<(), GlesError> {
        if self
            .texture
            .as_ref()
            .is_none_or(|texture| texture.size() != size)
        {
            let old = self.texture.take();
            give_back(&self.spares, old);
            self.texture = Some(take(renderer, &self.spares, size)?);
        }
        Ok(())
    }
}

fn give_back(spares: &Spares, texture: Option<GlesTexture>) {
    let mut spare = spares
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    spare.textures.extend(texture);
    let over = spare.textures.len().saturating_sub(SPARES);
    spare.textures.drain(..over);
}

impl Drop for Held {
    fn drop(&mut self) {
        let old = self.texture.take();
        give_back(&self.spares, old);
    }
}

#[derive(Debug)]
struct Program(GlesTexProgram);

fn program(renderer: &mut GlesRenderer) -> Result<GlesTexProgram, GlesError> {
    if let Some(program) = renderer.egl_context().user_data().get::<Program>() {
        return Ok(program.0.clone());
    }
    let compiled = renderer.compile_custom_texture_shader(
        include_str!("motion_blur.frag"),
        &[
            UniformName::new("blur_step_uv", UniformType::_2f),
            UniformName::new("blur_taps", UniformType::_1f),
        ],
    )?;
    renderer
        .egl_context()
        .user_data()
        .insert_if_missing(|| Program(compiled.clone()));
    Ok(compiled)
}

/// The pass over a run of windows that move as one.
#[derive(Debug, Clone)]
pub struct MotionBlurElement {
    id: Id,
    commit: CommitCounter,
    /// The box round `regions`.
    geometry: Rectangle<i32, Physical>,
    /// What is redrawn: each window and its margin, on the output.
    regions: Vec<Rectangle<i32, Physical>>,
    /// In physical pixels.
    streak: [f32; 2],
    /// How far past `geometry` the copy goes, in physical pixels.
    padding: i32,
    program: GlesTexProgram,
    spares: Spares,
}

/// The element that draws `streak` over the windows drawn in `windows`, to
/// go in front of everything of the topmost of them.
pub fn element(
    renderer: &mut GlesRenderer,
    state: &mut WindowEffectElementState,
    streak: [f64; 2],
    windows: &[LogicalRect],
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Result<Option<MotionBlurElement>, GlesError> {
    let Some(plan) = plan(streak) else {
        return Ok(None);
    };
    let regions: Vec<_> = windows
        .iter()
        .filter(|window| window.width > 0 && window.height > 0)
        .filter_map(|window| {
            let rect = plan.rect(*window);
            Rectangle::new((rect.x, rect.y).into(), (rect.width, rect.height).into())
                .intersection(output_geo)
        })
        .map(|rect| {
            crate::backend::visual::logical_rect_to_physical_rect(
                LogicalRect::new(rect.loc.x, rect.loc.y, rect.size.w, rect.size.h),
                output_geo.loc,
                scale,
            )
        })
        .collect();
    let Some(geometry) = regions.iter().copied().reduce(Rectangle::merge) else {
        return Ok(None);
    };
    let streak = [plan.blur.x * scale.x as f32, plan.blur.y * scale.y as f32];
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for region in &regions {
        (region.loc.x, region.loc.y, region.size.w, region.size.h).hash(&mut hasher);
    }
    streak.map(f32::to_bits).hash(&mut hasher);
    let signature = hasher.finish();
    if state.signature != signature {
        state.signature = signature;
        state.commit_counter.increment();
    }
    Ok(Some(MotionBlurElement {
        id: state.id.clone(),
        commit: state.commit_counter,
        geometry,
        regions,
        streak,
        padding: (f64::from(plan.padding()) * scale.x.max(scale.y)).ceil() as i32,
        program: program(renderer)?,
        spares: spares(renderer),
    }))
}

/// The streak element to draw in front of `window` as `output` draws it,
/// if it is the topmost of a run of windows that move as one. They share
/// the pass: each pass is a copy and a draw of its own, and what a pass
/// costs is in their number, not their size.
pub fn element_for_window(
    renderer: &mut GlesRenderer,
    decorations: &mut HashMap<Window, WindowDecorationState>,
    windows_top_to_bottom: &[Window],
    window: &Window,
    output_name: &str,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Option<MotionBlurElement> {
    // Compiled with the first window drawn: the first move would wait a
    // frame or more on it.
    let _ = program(renderer);
    let decoration = decorations.get_mut(window)?;
    let streak = decoration.managed_window.motion_blur;
    if plan(streak).is_none() {
        if !decoration.window_effect_cache.is_empty() {
            decoration
                .window_effect_cache
                .remove(&format!("motion-blur@{output_name}"));
        }
        return None;
    }
    let cache = format!("motion-blur@{output_name}");
    let drawn: Vec<_> = windows_top_to_bottom
        .iter()
        .filter_map(|other| Some((other, decorations.get(other)?)))
        .filter(|(_, decoration)| !decoration.managed_window_hidden_on_output(output_name))
        .collect();
    let stack: Vec<_> = drawn
        .iter()
        .map(|(_, decoration)| {
            let streak = decoration.managed_window.motion_blur;
            plan(streak).map(|_| key(streak))
        })
        .collect();
    let run = drawn
        .iter()
        .position(|(other, _)| *other == window)
        .and_then(|index| run(&stack, index));
    let Some(run) = run else {
        decorations
            .get_mut(window)?
            .window_effect_cache
            .remove(&cache);
        return None;
    };
    let rects: Vec<_> = drawn[run]
        .iter()
        .map(|(_, decoration)| {
            crate::backend::visual::transformed_root_rect(
                decoration.layout.root.rect,
                decoration.visual_transform,
            )
        })
        .collect();
    let state = decorations
        .get_mut(window)?
        .window_effect_cache
        .entry(cache)
        .or_default();
    element(renderer, state, streak, &rects, output_geo, scale)
        .inspect_err(|error| tracing::warn!(?error, "failed to build a window's motion blur"))
        .ok()
        .flatten()
}

impl MotionBlurElement {
    fn capture_region(&self, geometry: Rectangle<i32, Physical>) -> Rectangle<i32, Physical> {
        Rectangle::new(
            Point::from((geometry.loc.x - self.padding, geometry.loc.y - self.padding)),
            (
                geometry.size.w + 2 * self.padding,
                geometry.size.h + 2 * self.padding,
            )
                .into(),
        )
    }
}

impl Element for MotionBlurElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(
            (
                f64::from(self.geometry.size.w),
                f64::from(self.geometry.size.h),
            )
                .into(),
        )
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }

    fn is_framebuffer_effect(&self) -> bool {
        true
    }

    fn framebuffer_capture_region(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.capture_region(self.geometry)
    }
}

impl RenderElement<GlesRenderer> for MotionBlurElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(held) = cache.and_then(|cache| cache.get::<RefCell<Held>>()) else {
            return Ok(());
        };
        let held = held.borrow();
        let Some(texture) = held.texture.as_ref() else {
            return Ok(());
        };
        let size = texture.size();
        let taps = taps(self.streak[0].hypot(self.streak[1])) as f32;
        // The copy lies as the framebuffer does, and the streak with it.
        let transform = frame.transformation();
        let output = frame.output_size();
        let area = Size::<f64, Physical>::from((f64::from(output.w), f64::from(output.h)));
        let turned = |x: f32, y: f32| {
            transform.transform_point_in(
                Point::<f64, Physical>::from((f64::from(x), f64::from(y))),
                &area,
            )
        };
        let (from, to) = (turned(0.0, 0.0), turned(self.streak[0], self.streak[1]));
        let (along, down) = ((to.x - from.x) as f32, (to.y - from.y) as f32);
        let uniforms = [
            Uniform::new(
                "blur_step_uv",
                [
                    along / taps / size.w.max(1) as f32,
                    down / taps / size.h.max(1) as f32,
                ],
            ),
            Uniform::new("blur_taps", taps),
        ];
        let timing =
            begin_gpu_timing_frame_span(frame, "motion-blur-draw", (dst.size.w, dst.size.h));
        let mut result = Ok(());
        for region in &self.regions {
            let within = Rectangle::new(region.loc - self.geometry.loc, region.size);
            let Some(damage) = relative_intersections(damage, within) else {
                continue;
            };
            // The texture is the output, so a region reads from where it is.
            let from = transform.transform_rect_in(*region, &output);
            let src = Rectangle::new(
                Point::from((f64::from(from.loc.x), f64::from(from.loc.y))),
                (f64::from(from.size.w), f64::from(from.size.h)).into(),
            );
            result = frame.render_texture_from_to(
                texture,
                src,
                Rectangle::new(dst.loc + within.loc, within.size),
                &damage,
                &[],
                transform.invert(),
                1.0,
                Some(&self.program),
                &uniforms,
            );
            if result.is_err() {
                break;
            }
        }
        end_gpu_timing_frame_span(frame, timing);
        result
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        let output = Rectangle::from_size(frame.output_size());
        let Some(capture) = self.capture_region(dst).intersection(output) else {
            return Ok(());
        };
        // `capture` is in the output's own upright space; the framebuffer
        // has its pixels as the output's transform lays them out, and the
        // copy keeps them so: turning them upright is a pass of its own.
        let transform = frame.transformation();
        let blit = transform.transform_rect_in(capture, &output.size);
        let size = transform.transform_size(output.size);
        let held = cache.get_or_insert::<RefCell<Held>, _>(|| {
            RefCell::new(Held {
                texture: None,
                spares: self.spares.clone(),
            })
        });
        let mut held = held.borrow_mut();
        held.fit(frame.renderer().as_mut(), (size.w, size.h).into())?;
        let target = held
            .texture
            .as_ref()
            .expect("fit made the texture")
            .tex_id();
        frame.with_context(|gl| unsafe {
            with_gpu_timing_gl_span(
                gl,
                "motion-blur-capture",
                (blit.size.w, blit.size.h),
                || {
                    // What reads the frame's own target, whatever an effect
                    // earlier in the frame left bound for reading.
                    let (mut drawn_to, mut read_from) = (0i32, 0i32);
                    gl.GetIntegerv(ffi::DRAW_FRAMEBUFFER_BINDING, &mut drawn_to);
                    gl.GetIntegerv(ffi::READ_FRAMEBUFFER_BINDING, &mut read_from);
                    gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, drawn_to as u32);
                    gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, ensure_blur_scratch_fbo(gl));
                    gl.FramebufferTexture2D(
                        ffi::DRAW_FRAMEBUFFER,
                        ffi::COLOR_ATTACHMENT0,
                        ffi::TEXTURE_2D,
                        target,
                        0,
                    );
                    gl.Disable(ffi::SCISSOR_TEST);
                    let (left, top) = (blit.loc.x, blit.loc.y);
                    let (right, bottom) = (left + blit.size.w, top + blit.size.h);
                    gl.BlitFramebuffer(
                        left,
                        top,
                        right,
                        bottom,
                        left,
                        top,
                        right,
                        bottom,
                        ffi::COLOR_BUFFER_BIT,
                        ffi::NEAREST,
                    );
                    gl.Enable(ffi::SCISSOR_TEST);
                    gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, read_from as u32);
                    gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, drawn_to as u32);
                },
            );
        })?;
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
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
        // Its taps read no further than the copy goes.
        assert!(plan.padding() as f32 >= plan.blur.length() / 2.0);
    }

    /// The windows the passes of `stack` cover, by the window each pass is
    /// in front of.
    fn passes(stack: &[Option<[i64; 2]>]) -> Vec<(usize, std::ops::Range<usize>)> {
        (0..stack.len())
            .filter_map(|index| Some((index, run(stack, index)?)))
            .collect()
    }

    #[test]
    fn a_tape_is_one_pass_in_front_of_its_topmost_window() {
        let tape = Some(key([9.0, 0.0]));
        assert_eq!(passes(&[tape, tape, tape]), [(0, 0..3)]);
        // Under a float that is still.
        assert_eq!(passes(&[None, tape, tape]), [(1, 1..3)]);
        assert_eq!(passes(&[None, None]), []);
    }

    #[test]
    fn a_still_window_in_the_stack_keeps_the_pass_above_it_off_the_windows_below() {
        // A fullscreen window leaving, a pinned window that stays, and the
        // tiles of the workspace coming in under it.
        let switch = Some(key([0.0, -11.0]));
        assert_eq!(
            passes(&[switch, None, switch, switch]),
            [(0, 0..1), (2, 2..4)]
        );
    }

    #[test]
    fn windows_with_streaks_of_their_own_get_passes_of_their_own() {
        let (tape, flick) = (Some(key([9.0, 0.0])), Some(key([3.0, 4.0])));
        assert_eq!(passes(&[flick, tape, tape]), [(0, 0..1), (1, 1..3)]);
        assert_eq!(
            passes(&[tape, flick, tape]),
            [(0, 0..1), (1, 1..2), (2, 2..3)]
        );
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

    smithay::render_elements! {
        Drawn<=GlesRenderer>;
        Blur=MotionBlurElement,
        Under=smithay::backend::renderer::element::texture::TextureRenderElement<smithay::backend::renderer::gles::GlesTexture>,
    }

    const DESK: (i32, i32) = (640, 400);

    /// What lies under the windows: blocks of colour, hairlines and rows
    /// the size of text, so a pass a pixel out of place shows.
    fn desk() -> Vec<u8> {
        let mut pixels = Vec::with_capacity((DESK.0 * DESK.1 * 4) as usize);
        for y in 0..DESK.1 {
            for x in 0..DESK.0 {
                let block = ((x / 8) as u32).wrapping_mul(2_654_435_761)
                    ^ ((y / 8) as u32).wrapping_mul(40_503);
                let mut colour = [
                    (block >> 8) as u8 / 2,
                    (block >> 16) as u8 / 2,
                    (block >> 24) as u8 / 2,
                    255,
                ];
                if x % 16 == 0 || y % 16 == 0 || (y % 12 > 6 && x % 7 < 3) {
                    colour = [235, 235, 235, 255];
                }
                pixels.extend(colour);
            }
        }
        pixels
    }

    /// A tape scroll, a workspace switch and a flick on a diagonal: two
    /// windows side by side that speed up and brake, onto and off the
    /// output, with the streak formalwm gives those speeds.
    fn moves() -> Vec<([f64; 2], Vec<LogicalRect>)> {
        let mut frames = Vec::new();
        for (way, from) in [
            ((1.0, 0.0), (-260.0, 20.0)),
            ((0.0, -1.0), (40.0, 380.0)),
            ((0.6, 0.8), (-100.0, -100.0)),
        ] {
            let mut travelled = 0.0;
            for step in 0..20 {
                let speed =
                    11_000.0 * (std::f64::consts::PI * (f64::from(step) + 0.5) / 20.0).sin();
                let length = (speed / 960.0 - 0.75).clamp(0.0, 12.0);
                travelled += speed / 240.0;
                let at = (
                    (from.0 + way.0 * travelled).round() as i32,
                    (from.1 + way.1 * travelled).round() as i32,
                );
                frames.push((
                    [way.0 * length, way.1 * length],
                    vec![
                        LogicalRect::new(at.0, at.1, 300, 200),
                        LogicalRect::new(at.0 + 320, at.1, 300, 200),
                    ],
                ));
            }
        }
        frames
    }

    /// The box filter along `streak` of `desk` under `windows`, worked out
    /// here: what the pass has to put on the screen, and which of its
    /// bytes the pass may touch at all.
    fn filtered(desk: &[u8], streak: [f64; 2], windows: &[LogicalRect]) -> (Vec<u8>, Vec<bool>) {
        let mut out = desk.to_vec();
        let mut passed = vec![false; desk.len()];
        let Some(plan) = plan(streak) else {
            return (out, passed);
        };
        let taps = taps(plan.blur.length());
        let step = (
            f64::from(plan.blur.x) / f64::from(taps),
            f64::from(plan.blur.y) / f64::from(taps),
        );
        let texel = |x: i32, y: i32, channel: usize| {
            let (x, y) = (x.clamp(0, DESK.0 - 1), y.clamp(0, DESK.1 - 1));
            f64::from(desk[((y * DESK.0 + x) * 4) as usize + channel])
        };
        let sample = |x: f64, y: f64, channel: usize| {
            let (u, v) = (x - 0.5, y - 0.5);
            let (left, top) = (u.floor(), v.floor());
            let (fx, fy) = (u - left, v - top);
            let (left, top) = (left as i32, top as i32);
            texel(left, top, channel) * (1.0 - fx) * (1.0 - fy)
                + texel(left + 1, top, channel) * fx * (1.0 - fy)
                + texel(left, top + 1, channel) * (1.0 - fx) * fy
                + texel(left + 1, top + 1, channel) * fx * fy
        };
        for window in windows {
            let rect = plan.rect(*window);
            for y in rect.y.max(0)..(rect.y + rect.height).min(DESK.1) {
                for x in rect.x.max(0)..(rect.x + rect.width).min(DESK.0) {
                    for channel in 0..4 {
                        let sum: f64 = (0..taps)
                            .map(|tap| {
                                let along = f64::from(tap) - f64::from(taps - 1) / 2.0;
                                sample(
                                    f64::from(x) + 0.5 + along * step.0,
                                    f64::from(y) + 0.5 + along * step.1,
                                    channel,
                                )
                            })
                            .sum();
                        let at = ((y * DESK.0 + x) * 4) as usize + channel;
                        out[at] = (sum / f64::from(taps)).round() as u8;
                        passed[at] = true;
                    }
                }
            }
        }
        (out, passed)
    }

    /// Draws `moves()` over the desk frame after frame, as an output does:
    /// one buffer, the damage tracker, the element's state kept between
    /// frames. With `SHOJI_MOTION_BLUR_DUMP` set to a directory, every
    /// frame and its damage is written there.
    #[test]
    #[ignore = "requires surfaceless EGL; renders only offscreen buffers"]
    fn the_pass_is_the_box_filter_of_what_is_under_the_windows() {
        use smithay::{
            backend::{
                allocator::Fourcc,
                egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
                renderer::{
                    Bind, ExportMem, ImportMem, Offscreen, Renderer,
                    damage::OutputDamageTracker,
                    element::{Id, Kind, texture::TextureRenderElement},
                    gles::GlesTexture,
                },
            },
            utils::Transform,
        };

        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let desk = desk();
        let desk_texture = renderer
            .import_memory(&desk, Fourcc::Abgr8888, DESK.into(), false)
            .unwrap();
        let (under_id, context) = (Id::new(), renderer.context_id());
        let under = |texture: &GlesTexture| {
            TextureRenderElement::from_static_texture(
                under_id.clone(),
                context.clone(),
                (0.0, 0.0),
                texture.clone(),
                1,
                Transform::Normal,
                None,
                None,
                None,
                None,
                Kind::Unspecified,
            )
        };
        let output_geo = Rectangle::from_size(DESK.into());
        let dump = std::env::var_os("SHOJI_MOTION_BLUR_DUMP").map(std::path::PathBuf::from);
        if let Some(dump) = &dump {
            std::fs::create_dir_all(dump).unwrap();
        }
        // A frame at rest first and last: the desk alone.
        let mut frames = vec![([0.0, 0.0], Vec::new())];
        frames.extend(moves());
        frames.push(([0.0, 0.0], Vec::new()));
        let spares = spares(&mut renderer);
        // As a monitor is, as the nested window is, and on its side.
        for transform in [Transform::Normal, Transform::Flipped180, Transform::_90] {
            let made = spares.lock().unwrap().made;
            let size = transform.transform_size(Size::<i32, Physical>::from(DESK));
            let mut target: GlesTexture = Offscreen::<GlesTexture>::create_buffer(
                &mut renderer,
                Fourcc::Abgr8888,
                (size.w, size.h).into(),
            )
            .unwrap();
            let mut tracker = OutputDamageTracker::new((size.w, size.h), 1.0, transform);
            let mut state = WindowEffectElementState::default();
            // Where each of the desk's pixels is in what is read back, top
            // row first or last: whichever shows the desk at rest.
            let place = |transform: Transform, upside_down: bool| -> Vec<usize> {
                (0..DESK.1)
                    .flat_map(|y| (0..DESK.0).map(move |x| (x, y)))
                    .map(|(x, y)| {
                        let at = transform
                            .transform_rect_in(
                                Rectangle::<i32, Physical>::new((x, y).into(), (1, 1).into()),
                                &Size::from(DESK),
                            )
                            .loc;
                        let row = if upside_down { size.h - 1 - at.y } else { at.y };
                        ((row * size.w + at.x) * 4) as usize
                    })
                    .collect()
            };
            let mut places = place(transform, false);
            for (index, (streak, windows)) in frames.iter().enumerate() {
                let blur = element(
                    &mut renderer,
                    &mut state,
                    *streak,
                    windows,
                    output_geo,
                    Scale::from(1.0),
                )
                .unwrap();
                let elements: Vec<Drawn> = blur
                    .into_iter()
                    .map(Drawn::Blur)
                    .chain([Drawn::Under(under(&desk_texture))])
                    .collect();
                let damage = {
                    let mut framebuffer = renderer.bind(&mut target).unwrap();
                    let age = usize::from(index > 0);
                    let result = tracker
                        .render_output(&mut renderer, &mut framebuffer, age, &elements, [0.0; 4])
                        .unwrap();
                    result.damage.cloned().unwrap_or_default()
                };
                let mapping = renderer
                    .copy_texture(
                        &target,
                        Rectangle::from_size((size.w, size.h).into()),
                        Fourcc::Abgr8888,
                    )
                    .unwrap();
                let read = renderer.map_texture(&mapping).unwrap().to_vec();
                let upright = |places: &[usize]| -> Vec<u8> {
                    places
                        .iter()
                        .flat_map(|at| read[*at..*at + 4].iter().copied())
                        .collect()
                };
                if index == 0 {
                    places = [transform, transform.invert()]
                        .into_iter()
                        .flat_map(|way| [place(way, false), place(way, true)])
                        .find(|places| upright(places) == desk)
                        .expect("the desk at rest is read back as it is");
                }
                let drawn = upright(&places);
                if let Some(dump) = dump.as_ref().filter(|_| transform == Transform::Normal) {
                    std::fs::write(dump.join(format!("{index:03}.rgba")), &drawn).unwrap();
                    std::fs::write(
                        dump.join(format!("{index:03}.damage")),
                        format!("{damage:?}"),
                    )
                    .unwrap();
                }
                let (wanted, passed) = filtered(&desk, *streak, windows);
                let worst = drawn
                    .iter()
                    .zip(&wanted)
                    .map(|(drawn, wanted)| drawn.abs_diff(*wanted))
                    .max()
                    .unwrap();
                assert!(
                    worst <= 2,
                    "{transform:?}, frame {index}, streak {streak:?}: {worst} levels out"
                );
                // Where no window is, the desk to the bit.
                let untouched = drawn
                    .iter()
                    .zip(&desk)
                    .zip(&passed)
                    .all(|((drawn, desk), passed)| *passed || drawn == desk);
                assert!(
                    untouched,
                    "{transform:?}, frame {index}, streak {streak:?}: drew outside the windows"
                );
            }
            // One texture at most for the whole of it: the scroll's group
            // shrinks as it leaves the output and every window changes
            // place every frame.
            assert!(spares.lock().unwrap().made <= made + 1, "{transform:?}");
        }
        assert_eq!(spares.lock().unwrap().made, 2);
    }
}
