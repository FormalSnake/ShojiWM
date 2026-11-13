//! A managed window's `motion_blur`: its streak, drawn as one pass over the
//! pixels it and the windows moving with it cover, once they and everything
//! under them are in the framebuffer. The window itself is drawn as at
//! rest, so a streak that comes and goes changes nothing else about its
//! frame.
//!
//! The pass is a copy of the output's pixels under the windows into a
//! texture, then one draw per window that reads the copy along the streak.
//! The texture is as large as the output and every pass on it shares the
//! one: each copies just before it draws, and one that finds another's
//! copy in it copies again.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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

/// How long a copy of a size nothing has drawn at is kept.
const IDLE: Duration = Duration::from_secs(10);

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
        // Half the streak lands on either side of an edge, and nothing
        // further out has a moving pixel in it.
        reach: (length * keep / 2.0).ceil() as i32,
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

/// A window an output draws: the key of its streak, `None` for one that is
/// still, and the rect it is drawn in, with the pass's margin if it moves.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Lying {
    key: Option<[i64; 2]>,
    rect: LogicalRect,
}

fn overlap(a: LogicalRect, b: LogicalRect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

/// The passes for `stack`, which is what an output draws, top to bottom:
/// for each the windows it covers, the one it is drawn in front of first.
/// Windows with one streak share a pass. Another window between them in
/// the stack ends the run only where it lies over one of those below it:
/// the pass of those above would blur it there.
fn runs(stack: &[Lying]) -> Vec<Vec<usize>> {
    struct Run {
        key: [i64; 2],
        windows: Vec<usize>,
        /// The others drawn under its topmost window so far.
        between: Vec<LogicalRect>,
    }
    let mut open: Vec<Run> = Vec::new();
    let mut runs = Vec::new();
    for (index, lying) in stack.iter().enumerate() {
        if let Some(key) = lying.key {
            let run = open.iter().position(|run| run.key == key);
            let covered = run.is_some_and(|run| {
                open[run]
                    .between
                    .iter()
                    .any(|other| overlap(*other, lying.rect))
            });
            match run {
                Some(run) if !covered => open[run].windows.push(index),
                _ => {
                    runs.extend(run.map(|run| open.remove(run).windows));
                    open.push(Run {
                        key,
                        windows: vec![index],
                        between: Vec::new(),
                    });
                }
            }
        }
        for run in &mut open {
            if Some(run.key) != lying.key {
                run.between.push(lying.rect);
            }
        }
    }
    runs.extend(open.into_iter().map(|run| run.windows));
    runs.sort();
    runs
}

/// A copy of an output's pixels, laid out as its framebuffer has them.
#[derive(Debug)]
struct Kept {
    texture: GlesTexture,
    /// The pass whose copy is in it, and where in the target it was
    /// drawing its box: a copy of part of the output lies elsewhere.
    of: Option<(Id, Point<i32, Physical>)>,
    used: Instant,
}

/// The copies, kept with the GL context: one for each size drawn at, an
/// output's or a copy of part of one.
#[derive(Debug, Default)]
struct Copies {
    held: Vec<Kept>,
    /// How many were ever made.
    made: usize,
}

type Shared = Arc<Mutex<Copies>>;

fn copies(renderer: &mut GlesRenderer) -> Shared {
    let data = renderer.egl_context().user_data();
    data.insert_if_missing(Shared::default);
    data.get::<Shared>()
        .expect("the copies were just put there")
        .clone()
}

impl Copies {
    /// The copy for a framebuffer of `size`, made if there is none.
    fn fit(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Buffer>,
    ) -> Result<&mut Kept, GlesError> {
        let now = Instant::now();
        // An output that changed mode or went, a region no longer copied.
        self.held
            .retain(|copy| copy.texture.size() == size || now.duration_since(copy.used) < IDLE);
        let at = match self
            .held
            .iter()
            .position(|copy| copy.texture.size() == size)
        {
            Some(at) => at,
            None => {
                let texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, size)?;
                // A new texture repeats: a tap past the output's edge would
                // read its far side.
                let id = texture.tex_id();
                renderer.with_context(|gl| unsafe {
                    gl.BindTexture(ffi::TEXTURE_2D, id);
                    for side in [ffi::TEXTURE_WRAP_S, ffi::TEXTURE_WRAP_T] {
                        gl.TexParameteri(ffi::TEXTURE_2D, side, ffi::CLAMP_TO_EDGE as i32);
                    }
                    gl.BindTexture(ffi::TEXTURE_2D, 0);
                })?;
                self.made += 1;
                self.held.push(Kept {
                    texture,
                    of: None,
                    used: now,
                });
                self.held.len() - 1
            }
        };
        self.held[at].used = now;
        Ok(&mut self.held[at])
    }
}

/// Every copy of this GL context goes: an output of its has gone or has
/// changed mode, and a copy of the size it drew at would stay otherwise
/// until some pass copied again.
pub fn forget_copies(renderer: &mut GlesRenderer) {
    copies(renderer)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .held
        .clear();
}

/// The passes of one frame of an output, counted over every windows node
/// the frame draws: what one node cannot see of the others.
#[derive(Debug, Clone, Default)]
pub struct Frame(Arc<AtomicUsize>);

impl Frame {
    /// `passes` more are in the frame.
    pub fn add(&self, passes: usize) {
        self.0.fetch_add(passes, Ordering::Relaxed);
    }

    fn crowded(&self) -> bool {
        self.0.load(Ordering::Relaxed) > 1
    }
}

/// A commit no pass has had: one more for every pass built.
fn fresh_commit() -> CommitCounter {
    static BUILT: AtomicUsize = AtomicUsize::new(0);
    CommitCounter::from((usize::MAX / 2).wrapping_add(BUILT.fetch_add(1, Ordering::Relaxed)))
}

/// `None` where the shader does not compile, found out once.
#[derive(Debug)]
struct Program(Option<GlesTexProgram>);

fn program(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    if let Some(program) = renderer.egl_context().user_data().get::<Program>() {
        return program.0.clone();
    }
    let compiled = renderer
        .compile_custom_texture_shader(
            include_str!("motion_blur.frag"),
            &[
                UniformName::new("blur_step_uv", UniformType::_2f),
                UniformName::new("blur_taps", UniformType::_1f),
            ],
        )
        .inspect_err(|error| tracing::warn!(?error, "the motion blur shader does not compile"))
        .ok();
    renderer
        .egl_context()
        .user_data()
        .insert_if_missing(|| Program(compiled.clone()));
    compiled
}

/// The pass over a run of windows that move as one.
#[derive(Debug, Clone)]
pub struct MotionBlurElement {
    id: Id,
    commit: CommitCounter,
    /// Its commit in a frame with other passes in it: they share the copy,
    /// so each has to copy again, which a new commit makes the damage
    /// tracker ask for. Known only once every node of the frame is built.
    among_others: CommitCounter,
    frame: Frame,
    /// The box round `regions`.
    geometry: Rectangle<i32, Physical>,
    /// What is redrawn: each window and its margin, on the output.
    regions: Vec<Rectangle<i32, Physical>>,
    /// In physical pixels.
    streak: [f32; 2],
    /// How far past `geometry` the copy goes, in physical pixels.
    padding: i32,
    program: GlesTexProgram,
    copies: Shared,
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
    // The size of the output's framebuffer.
    target: Size<i32, Physical>,
    frame: &Frame,
) -> Option<MotionBlurElement> {
    let plan = plan(streak)?;
    let program = program(renderer)?;
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
    let geometry = regions.iter().copied().reduce(Rectangle::merge)?;
    let streak = [plan.blur.x * scale.x as f32, plan.blur.y * scale.y as f32];
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for region in &regions {
        (region.loc.x, region.loc.y, region.size.w, region.size.h).hash(&mut hasher);
    }
    streak.map(f32::to_bits).hash(&mut hasher);
    let signature = hasher.finish();
    let copies = copies(renderer);
    // Another pass has copied into the output's texture since this one
    // did: it has to copy again. A copy of part of the output is in a
    // texture of its own size and says nothing of the output's.
    let kept = copies
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .held
        .iter()
        .any(|copy| {
            let size = copy.texture.size();
            [(size.w, size.h), (size.h, size.w)].contains(&(target.w, target.h))
                && copy.of.as_ref() == Some(&(state.id.clone(), geometry.loc))
        });
    if state.signature != signature || !kept {
        state.signature = signature;
        state.commit_counter.increment();
    }
    Some(MotionBlurElement {
        id: state.id.clone(),
        commit: state.commit_counter,
        among_others: fresh_commit(),
        frame: frame.clone(),
        geometry,
        regions,
        streak,
        padding: (f64::from(plan.padding()) * scale.x.max(scale.y)).ceil() as i32,
        program,
        copies,
    })
}

/// The windows an output's frame draws, top to bottom, as it comes to
/// them, and where in its elements each one's pass would go.
#[derive(Debug, Default)]
pub struct Stack {
    windows: Vec<(Window, usize)>,
}

impl Stack {
    /// `window` is drawn, and a pass in front of it goes at `at` in the
    /// frame's elements.
    pub fn reach(&mut self, renderer: &mut GlesRenderer, window: &Window, at: usize) {
        // Compiled with the first window drawn: the first move would wait
        // a frame or more on it.
        if self.windows.is_empty() {
            program(renderer);
        }
        self.windows.push((window.clone(), at));
    }

    /// The passes of the frame and where each goes in its elements, the
    /// last place first, so that putting one in moves none of the others.
    /// Windows that move as one share a pass: each pass is a copy and a
    /// draw of its own, and what a pass costs is in their number.
    pub fn passes(
        self,
        renderer: &mut GlesRenderer,
        decorations: &mut HashMap<Window, WindowDecorationState>,
        output_name: &str,
        output_geo: Rectangle<i32, Logical>,
        scale: Scale<f64>,
        // The size of the output's framebuffer.
        target: Size<i32, Physical>,
        frame: &Frame,
    ) -> Vec<(usize, MotionBlurElement)> {
        let cache = format!("motion-blur@{output_name}");
        let drawn: Vec<_> = self
            .windows
            .iter()
            .filter_map(|(window, at)| {
                let decoration = decorations.get(window)?;
                let rect = crate::backend::visual::transformed_root_rect(
                    decoration.layout.root.rect,
                    decoration.visual_transform,
                );
                Some((window, *at, decoration.managed_window.motion_blur, rect))
            })
            .collect();
        let stack: Vec<_> = drawn
            .iter()
            .map(|(_, _, streak, rect)| match plan(*streak) {
                Some(plan) => Lying {
                    key: Some(key(*streak)),
                    rect: plan.rect(*rect),
                },
                None => Lying {
                    key: None,
                    rect: *rect,
                },
            })
            .collect();
        let runs = runs(&stack);
        for (index, (window, ..)) in drawn.iter().enumerate() {
            if runs.iter().any(|run| run[0] == index) {
                continue;
            }
            if let Some(decoration) = decorations.get_mut(*window)
                && !decoration.window_effect_cache.is_empty()
            {
                decoration.window_effect_cache.remove(&cache);
            }
        }
        let mut passes: Vec<_> = runs
            .iter()
            .filter_map(|run| {
                let (window, at, streak, _) = drawn[run[0]];
                let rects: Vec<_> = run.iter().map(|index| drawn[*index].3).collect();
                let state = decorations
                    .get_mut(window)?
                    .window_effect_cache
                    .entry(cache.clone())
                    .or_default();
                Some((
                    at,
                    element(
                        renderer, state, streak, &rects, output_geo, scale, target, frame,
                    )?,
                ))
            })
            .collect();
        frame.add(passes.len());
        passes.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        passes
    }
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
        if self.frame.crowded() {
            self.among_others
        } else {
            self.commit
        }
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
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        // The copy lies as the framebuffer does, and the streak with it.
        let transform = frame.transformation();
        let output = frame.output_size();
        let size = transform.transform_size(output);
        let size = Size::<i32, Buffer>::from((size.w, size.h));
        // Not this pass's copy: what it would draw is another's pixels.
        let texture = self
            .copies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .held
            .iter()
            .find(|copy| {
                copy.texture.size() == size && copy.of.as_ref() == Some(&(self.id.clone(), dst.loc))
            })
            .map(|copy| copy.texture.clone());
        let Some(texture) = texture else {
            return Ok(());
        };
        let taps = taps(self.streak[0].hypot(self.streak[1])) as f32;
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
            // The texture is the target, so a region reads from where it
            // is drawn in it, which in a copy of part of the output is not
            // where it is on the output.
            let drawn = Rectangle::new(dst.loc + within.loc, within.size);
            let from = transform.transform_rect_in(drawn, &output);
            let src = Rectangle::new(
                Point::from((f64::from(from.loc.x), f64::from(from.loc.y))),
                (f64::from(from.size.w), f64::from(from.size.h)).into(),
            );
            result = frame.render_texture_from_to(
                &texture,
                src,
                drawn,
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
        _cache: &UserDataMap,
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
        let target = {
            let mut copies = self
                .copies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let copy = copies.fit(frame.renderer().as_mut(), (size.w, size.h).into())?;
            copy.of = Some((self.id.clone(), dst.loc));
            copy.texture.tex_id()
        };
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
    fn the_pass_reaches_no_further_than_the_streak_does() {
        let window = LogicalRect::new(100, 50, 800, 600);
        // Half the streak either side, and a still pixel 7 away is left.
        let twelve = plan([0.0, -12.0]).unwrap();
        assert_eq!(twelve.rect(window), LogicalRect::new(94, 44, 812, 612));
        assert_eq!(plan([0.6, 0.0]).unwrap().reach, 1);
        assert_eq!(plan([-9.0, 0.0]).unwrap().reach, 5);
        assert_eq!(plan([6.0, 8.0]).unwrap().reach, 5);
        for streak in [[0.6, 0.0], [-9.0, 0.0], [6.0, 8.0], [300.0, 0.0]] {
            let plan = plan(streak).unwrap();
            let half = plan.blur.length() / 2.0;
            assert!(plan.reach as f32 >= half && (plan.reach as f32) < half + 1.0);
            // Its taps read no further than the copy goes.
            assert!(plan.padding() as f32 >= half + 1.0);
        }
    }

    const TAPE: [f64; 2] = [9.0, 0.0];
    const SWITCH: [f64; 2] = [0.0, -11.0];

    /// A window `wide` by 300 at `x`, `y`, moving with `streak` if any.
    fn lying(streak: Option<[f64; 2]>, x: i32, y: i32, wide: i32) -> Lying {
        let rect = LogicalRect::new(x, y, wide, 300);
        match streak {
            Some(streak) => Lying {
                key: Some(key(streak)),
                rect: plan(streak).unwrap().rect(rect),
            },
            None => Lying { key: None, rect },
        }
    }

    #[test]
    fn a_tape_is_one_pass_in_front_of_its_topmost_window() {
        let tape = |x| lying(Some(TAPE), x, 0, 300);
        assert_eq!(runs(&[tape(0), tape(320), tape(640)]), [[0, 1, 2]]);
        // Under a float that is still.
        let float = lying(None, 100, 100, 300);
        assert_eq!(runs(&[float, tape(0), tape(320)]), [[1, 2]]);
        assert!(runs(&[float, lying(None, 0, 0, 300)]).is_empty());
    }

    #[test]
    fn a_still_window_over_one_that_moves_keeps_the_pass_above_it_off_that_one() {
        // A fullscreen window leaving, a pinned window that stays, and the
        // tiles of the workspace coming in under it.
        let leaving = lying(Some(SWITCH), 0, -400, 1000);
        let pinned = lying(None, 600, 500, 300);
        let (under, beside) = (
            lying(Some(SWITCH), 500, 400, 480),
            lying(Some(SWITCH), 0, 400, 480),
        );
        assert_eq!(
            runs(&[leaving, pinned, under, beside]),
            [vec![0], vec![2, 3]]
        );
        // Over none of them, it is in nobody's way.
        let aside = lying(None, 1200, 500, 300);
        assert_eq!(runs(&[leaving, aside, under, beside]), [[0, 2, 3]]);
    }

    #[test]
    fn windows_with_streaks_of_their_own_get_passes_of_their_own() {
        let tape = |x| lying(Some(TAPE), x, 0, 300);
        let flick = |x| lying(Some([3.0, 4.0]), x, 100, 300);
        assert_eq!(runs(&[flick(0), tape(0), tape(320)]), [vec![0], vec![1, 2]]);
        // Between two of a tape and over the lower one.
        assert_eq!(
            runs(&[tape(0), flick(320), tape(320)]),
            [vec![0], vec![1], vec![2]]
        );
    }

    #[test]
    fn two_workspaces_on_their_own_tapes_are_two_passes_however_they_are_stacked() {
        // A switch while the workspace leaving scrolls: its tiles and the
        // tiles coming in take turns in the stack, a screen apart.
        let leaving = |x| lying(Some([9.0, -11.0]), x, 0, 300);
        let coming = |x| lying(Some(SWITCH), x, 800, 300);
        let stack = [
            leaving(640),
            coming(640),
            leaving(320),
            coming(320),
            leaving(0),
            coming(0),
        ];
        assert_eq!(runs(&stack), [[0, 2, 4], [1, 3, 5]]);
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
        let copies = copies(&mut renderer);
        // As a monitor is, as the nested window is, and on its side.
        for transform in [Transform::Normal, Transform::Flipped180, Transform::_90] {
            let made = copies.lock().unwrap().made;
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
                    size,
                    &Frame::default(),
                );
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
            assert!(copies.lock().unwrap().made <= made + 1, "{transform:?}");
        }
        assert_eq!(copies.lock().unwrap().made, 2);
    }

    /// A switch while the workspace leaving scrolls: two workspaces of
    /// three tiles on streaks of their own, taking turns in the stack, and
    /// something above them that moves between frames in which they do not.
    #[test]
    #[ignore = "requires surfaceless EGL; renders only offscreen buffers"]
    fn two_workspaces_at_speeds_of_their_own_share_one_copy() {
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
        const MARK: i32 = 40;
        let red = [200, 30, 30, 255].repeat((MARK * MARK) as usize);
        let mark_texture = renderer
            .import_memory(&red, Fourcc::Abgr8888, (MARK, MARK).into(), false)
            .unwrap();
        let (under_id, mark_id, context) = (Id::new(), Id::new(), renderer.context_id());
        let shown = |id: &Id, texture: &GlesTexture, at: (f64, f64)| {
            TextureRenderElement::from_static_texture(
                id.clone(),
                context.clone(),
                at,
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
        let mut target: GlesTexture =
            Offscreen::<GlesTexture>::create_buffer(&mut renderer, Fourcc::Abgr8888, DESK.into())
                .unwrap();
        let mut tracker = OutputDamageTracker::new(DESK, 1.0, Transform::Normal);
        let mut states: HashMap<usize, WindowEffectElementState> = HashMap::new();
        let (leaving, coming) = ([9.0, -11.0], [0.0, -11.0]);
        let mut first = true;
        for step in 0..12 {
            let rects = |streak: [f64; 2], y: i32| -> Vec<LogicalRect> {
                (0..3)
                    .map(|tile| {
                        LogicalRect::new(
                            20 + 170 * tile + (streak[0] * f64::from(step)) as i32,
                            y - 6 * step,
                            150,
                            100,
                        )
                    })
                    .collect()
            };
            let (left, came) = (rects(leaving, 90), rects(coming, 260));
            let stack: Vec<Lying> = (0..3)
                .flat_map(|tile| {
                    [(leaving, left[tile]), (coming, came[tile])].map(|(streak, rect)| Lying {
                        key: Some(key(streak)),
                        rect: plan(streak).unwrap().rect(rect),
                    })
                })
                .collect();
            let runs = runs(&stack);
            assert_eq!(runs, [[0, 2, 4], [1, 3, 5]]);
            // The lower pass first: the upper one reads what it drew.
            let (wanted, passed) = filtered(&desk, coming, &came);
            let (wanted, over) = filtered(&wanted, leaving, &left);
            // The mark moves on its own while the tiles do not: twice over
            // those coming in, then twice over those leaving, whose pass is
            // the upper one and has nothing under it changing.
            for mark in [(60, 250), (64, 244), (230, 80), (234, 86)]
                .map(|(x, y)| (x + 9 * step, y - 6 * step))
            {
                let mut elements = vec![Drawn::Under(shown(
                    &mark_id,
                    &mark_texture,
                    (f64::from(mark.0), f64::from(mark.1)),
                ))];
                // Built as two windows nodes of one frame build them: each
                // knows of its own pass alone.
                let frame = Frame::default();
                for (run, streak, rects) in [(&runs[0], leaving, &left), (&runs[1], coming, &came)]
                {
                    let state = states.entry(run[0]).or_default();
                    let blur = element(
                        &mut renderer,
                        state,
                        streak,
                        rects,
                        output_geo,
                        Scale::from(1.0),
                        DESK.into(),
                        &frame,
                    );
                    frame.add(1);
                    elements.push(Drawn::Blur(blur.expect("the tiles are on the output")));
                }
                elements.push(Drawn::Under(shown(&under_id, &desk_texture, (0.0, 0.0))));
                {
                    let mut framebuffer = renderer.bind(&mut target).unwrap();
                    tracker
                        .render_output(
                            &mut renderer,
                            &mut framebuffer,
                            usize::from(!first),
                            &elements,
                            [0.0; 4],
                        )
                        .unwrap();
                }
                first = false;
                let mapping = renderer
                    .copy_texture(&target, Rectangle::from_size(DESK.into()), Fourcc::Abgr8888)
                    .unwrap();
                let read = renderer.map_texture(&mapping).unwrap().to_vec();
                let row = (DESK.0 * 4) as usize;
                // Top row first or last, whichever the mark is found in.
                let at = |x: i32, y: i32| ((y * DESK.0 + x) * 4) as usize;
                let mut drawn = read.clone();
                if drawn[at(mark.0, mark.1)..][..4] != red[..4] {
                    drawn = read.chunks(row).rev().flatten().copied().collect();
                }
                for y in 0..DESK.1 {
                    for x in 0..DESK.0 {
                        let marked = (mark.0..mark.0 + MARK).contains(&x)
                            && (mark.1..mark.1 + MARK).contains(&y);
                        for channel in 0..4 {
                            let at = at(x, y) + channel;
                            let wanted = if marked { red[channel] } else { wanted[at] };
                            let out = drawn[at].abs_diff(wanted);
                            assert!(
                                out <= 2,
                                "step {step}, mark {mark:?}, {x} {y}: {out} levels out"
                            );
                            assert!(
                                marked || passed[at] || over[at] || drawn[at] == desk[at],
                                "step {step}, mark {mark:?}, {x} {y}: drew outside the windows"
                            );
                        }
                    }
                }
            }
        }
        // Two passes a frame for the whole of it, and one texture.
        assert_eq!(copies(&mut renderer).lock().unwrap().made, 1);
    }

    /// A copy of part of the output, as a region screenshot takes it while
    /// the windows move: every element moved by the region's corner, drawn
    /// into a buffer the region's size on a tracker of its own.
    #[test]
    #[ignore = "requires surfaceless EGL; renders only offscreen buffers"]
    fn a_copy_of_part_of_the_output_shows_the_pass_as_the_output_does() {
        use smithay::{
            backend::{
                allocator::Fourcc,
                egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
                renderer::{
                    Bind, ExportMem, ImportMem, Offscreen, Renderer,
                    damage::OutputDamageTracker,
                    element::{
                        Id, Kind,
                        texture::TextureRenderElement,
                        utils::{Relocate, RelocateRenderElement},
                    },
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
        let output_geo = Rectangle::from_size(DESK.into());
        const PART: (i32, i32, i32, i32) = (100, 60, 320, 220);
        let mut whole: GlesTexture =
            Offscreen::<GlesTexture>::create_buffer(&mut renderer, Fourcc::Abgr8888, DESK.into())
                .unwrap();
        let mut part: GlesTexture = Offscreen::<GlesTexture>::create_buffer(
            &mut renderer,
            Fourcc::Abgr8888,
            (PART.2, PART.3).into(),
        )
        .unwrap();
        let mut on_output = OutputDamageTracker::new(DESK, 1.0, Transform::Normal);
        let mut on_part = OutputDamageTracker::new((PART.2, PART.3), 1.0, Transform::Normal);
        let mut state = WindowEffectElementState::default();
        let mut upside_down = false;
        let mut frames = vec![([0.0, 0.0], Vec::new())];
        frames.extend(moves());
        for (index, (streak, windows)) in frames.iter().enumerate() {
            let blur = element(
                &mut renderer,
                &mut state,
                *streak,
                windows,
                output_geo,
                Scale::from(1.0),
                DESK.into(),
                &Frame::default(),
            );
            let elements: Vec<Drawn> = blur
                .into_iter()
                .map(Drawn::Blur)
                .chain([Drawn::Under(TextureRenderElement::from_static_texture(
                    under_id.clone(),
                    context.clone(),
                    (0.0, 0.0),
                    desk_texture.clone(),
                    1,
                    Transform::Normal,
                    None,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                ))])
                .collect();
            let age = usize::from(index > 0);
            {
                let mut framebuffer = renderer.bind(&mut whole).unwrap();
                on_output
                    .render_output(&mut renderer, &mut framebuffer, age, &elements, [0.0; 4])
                    .unwrap();
            }
            let moved: Vec<_> = elements
                .iter()
                .map(|element| {
                    RelocateRenderElement::from_element(
                        element,
                        Point::<i32, Physical>::from((-PART.0, -PART.1)),
                        Relocate::Relative,
                    )
                })
                .collect();
            {
                let mut framebuffer = renderer.bind(&mut part).unwrap();
                on_part
                    .render_output(&mut renderer, &mut framebuffer, age, &moved, [0.0; 4])
                    .unwrap();
            }
            let mapping = renderer
                .copy_texture(
                    &part,
                    Rectangle::from_size((PART.2, PART.3).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            let read = renderer.map_texture(&mapping).unwrap().to_vec();
            let (wanted, _) = filtered(&desk, *streak, windows);
            let cut = |from: &[u8]| -> Vec<u8> {
                (PART.1..PART.1 + PART.3)
                    .flat_map(|y| {
                        let start = ((y * DESK.0 + PART.0) * 4) as usize;
                        from[start..start + (PART.2 * 4) as usize].iter().copied()
                    })
                    .collect()
            };
            let row = (PART.2 * 4) as usize;
            if index == 0 {
                upside_down = read != cut(&desk);
            }
            let drawn: Vec<u8> = if upside_down {
                read.chunks(row).rev().flatten().copied().collect()
            } else {
                read
            };
            // A tap past the part's edge has no pixel to read there, so
            // the streak's reach in from the edge is left out.
            let rim = plan(*streak).map_or(0, |plan| plan.padding());
            let wanted = cut(&wanted);
            let mut worst = 0;
            for y in rim..PART.3 - rim {
                for x in rim..PART.2 - rim {
                    let at = ((y * PART.2 + x) * 4) as usize;
                    for channel in 0..4 {
                        worst = worst.max(drawn[at + channel].abs_diff(wanted[at + channel]));
                    }
                }
            }
            assert!(
                worst <= 2,
                "frame {index}, streak {streak:?}: {worst} levels out"
            );
        }
    }

    /// Two outputs of one size with a pass each, a copy of the first one's
    /// corner taken in between, and then a frame of the first in which its
    /// pass holds still and only a mark above it moves.
    #[test]
    #[ignore = "requires surfaceless EGL; renders only offscreen buffers"]
    fn a_copy_of_the_outputs_corner_does_not_stand_for_the_outputs_own() {
        use smithay::{
            backend::{
                allocator::Fourcc,
                egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
                renderer::{
                    Bind, ExportMem, ImportMem, Offscreen, Renderer,
                    damage::OutputDamageTracker,
                    element::{
                        Id, Kind,
                        texture::TextureRenderElement,
                        utils::{Relocate, RelocateRenderElement},
                    },
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
        const MARK: i32 = 40;
        let red = [200, 30, 30, 255].repeat((MARK * MARK) as usize);
        let mark_texture = renderer
            .import_memory(&red, Fourcc::Abgr8888, (MARK, MARK).into(), false)
            .unwrap();
        let (under_id, mark_id, context) = (Id::new(), Id::new(), renderer.context_id());
        let shown = |id: &Id, texture: &GlesTexture, at: (f64, f64)| {
            TextureRenderElement::from_static_texture(
                id.clone(),
                context.clone(),
                at,
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
        let mut buffer = |size: (i32, i32)| -> GlesTexture {
            Offscreen::<GlesTexture>::create_buffer(&mut renderer, Fourcc::Abgr8888, size.into())
                .unwrap()
        };
        const PART: (i32, i32) = (320, 220);
        let (mut first, mut second, mut part) = (buffer(DESK), buffer(DESK), buffer(PART));
        let mut on_first = OutputDamageTracker::new(DESK, 1.0, Transform::Normal);
        let mut on_second = OutputDamageTracker::new(DESK, 1.0, Transform::Normal);
        let mut on_part = OutputDamageTracker::new(PART, 1.0, Transform::Normal);
        let (mut state, mut other) = Default::default();
        let streak = [9.0, 0.0];
        let windows = [LogicalRect::new(150, 120, 200, 150)];
        let (wanted, _) = filtered(&desk, streak, &windows);
        for (index, mark) in [(200, 150), (230, 170)].into_iter().enumerate() {
            let mut pass = |state: &mut WindowEffectElementState| {
                element(
                    &mut renderer,
                    state,
                    streak,
                    &windows,
                    output_geo,
                    Scale::from(1.0),
                    DESK.into(),
                    &Frame::default(),
                )
                .expect("the window is on the output")
            };
            let elements = [
                Drawn::Under(shown(
                    &mark_id,
                    &mark_texture,
                    (f64::from(mark.0), f64::from(mark.1)),
                )),
                Drawn::Blur(pass(&mut state)),
                Drawn::Under(shown(&under_id, &desk_texture, (0.0, 0.0))),
            ];
            let others = [
                Drawn::Blur(pass(&mut other)),
                Drawn::Under(shown(&under_id, &desk_texture, (0.0, 0.0))),
            ];
            let age = usize::from(index > 0);
            {
                let mut framebuffer = renderer.bind(&mut first).unwrap();
                on_first
                    .render_output(&mut renderer, &mut framebuffer, age, &elements, [0.0; 4])
                    .unwrap();
            }
            let mapping = renderer
                .copy_texture(&first, Rectangle::from_size(DESK.into()), Fourcc::Abgr8888)
                .unwrap();
            let read = renderer.map_texture(&mapping).unwrap().to_vec();
            // The first output's corner, then the second output's frame.
            let moved: Vec<_> = elements
                .iter()
                .map(|element| {
                    RelocateRenderElement::from_element(
                        element,
                        Point::<i32, Physical>::from((0, 0)),
                        Relocate::Relative,
                    )
                })
                .collect();
            {
                let mut framebuffer = renderer.bind(&mut part).unwrap();
                on_part
                    .render_output(&mut renderer, &mut framebuffer, age, &moved, [0.0; 4])
                    .unwrap();
            }
            {
                let mut framebuffer = renderer.bind(&mut second).unwrap();
                on_second
                    .render_output(&mut renderer, &mut framebuffer, age, &others, [0.0; 4])
                    .unwrap();
            }
            let at = |x: i32, y: i32| ((y * DESK.0 + x) * 4) as usize;
            let row = (DESK.0 * 4) as usize;
            let mut drawn = read.clone();
            if drawn[at(mark.0, mark.1)..][..4] != red[..4] {
                drawn = read.chunks(row).rev().flatten().copied().collect();
            }
            let mut worst = 0;
            for y in 0..DESK.1 {
                for x in 0..DESK.0 {
                    let marked = (mark.0..mark.0 + MARK).contains(&x)
                        && (mark.1..mark.1 + MARK).contains(&y);
                    for channel in 0..4 {
                        let at = at(x, y) + channel;
                        let wanted = if marked { red[channel] } else { wanted[at] };
                        worst = worst.max(drawn[at].abs_diff(wanted));
                    }
                }
            }
            assert!(worst <= 2, "frame {index}: {worst} levels out");
        }
    }

    #[test]
    #[ignore = "requires surfaceless EGL; renders only offscreen buffers"]
    fn an_output_that_goes_takes_the_copies_with_it() {
        use smithay::backend::egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay};

        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let context = EGLContext::new(&display).unwrap();
        let mut renderer = unsafe { GlesRenderer::new(context) }.unwrap();
        let copies = copies(&mut renderer);
        for size in [DESK, (DESK.1, 300)] {
            copies
                .lock()
                .unwrap()
                .fit(&mut renderer, size.into())
                .unwrap();
        }
        assert_eq!(copies.lock().unwrap().held.len(), 2);
        forget_copies(&mut renderer);
        assert!(copies.lock().unwrap().held.is_empty());
    }
}
