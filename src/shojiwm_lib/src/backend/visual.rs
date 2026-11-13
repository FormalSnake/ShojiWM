use smithay::{
    backend::renderer::{
        Renderer,
        element::{
            Element, Id, Kind, RenderElement, UnderlyingStorage,
            utils::{Relocate, RelocateRenderElement, RescaleRenderElement},
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{
        Buffer, Logical, Physical, Point, Rectangle, Scale, Transform, user_data::UserDataMap,
    },
};

use crate::ssd::{LogicalRect, WindowTransform};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnappedLogicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreciseLogicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub fn precise_rect_from_logical(rect: LogicalRect) -> PreciseLogicalRect {
    PreciseLogicalRect {
        x: rect.x as f32,
        y: rect.y as f32,
        width: rect.width as f32,
        height: rect.height as f32,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RectSnapMode {
    SharedEdges,
    OriginAndSize,
}

pub fn snapped_logical_rect_relative(
    rect: LogicalRect,
    origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> SnappedLogicalRect {
    snapped_logical_rect_relative_with_mode(rect, origin, scale, RectSnapMode::SharedEdges)
}

pub fn snapped_logical_rect_relative_with_mode(
    rect: LogicalRect,
    origin: Point<i32, Logical>,
    scale: Scale<f64>,
    mode: RectSnapMode,
) -> SnappedLogicalRect {
    let scale_x = scale.x.abs().max(0.0001);
    let scale_y = scale.y.abs().max(0.0001);
    let left = (((rect.x - origin.x) as f64) * scale_x).round() / scale_x;
    let top = (((rect.y - origin.y) as f64) * scale_y).round() / scale_y;
    let (right, bottom) = match mode {
        RectSnapMode::SharedEdges => (
            ((((rect.x + rect.width) - origin.x) as f64) * scale_x).round() / scale_x,
            ((((rect.y + rect.height) - origin.y) as f64) * scale_y).round() / scale_y,
        ),
        RectSnapMode::OriginAndSize => (
            left + ((rect.width as f64) * scale_x).round() / scale_x,
            top + ((rect.height as f64) * scale_y).round() / scale_y,
        ),
    };
    SnappedLogicalRect {
        x: left as f32,
        y: top as f32,
        width: (right - left).max(0.0) as f32,
        height: (bottom - top).max(0.0) as f32,
    }
}

pub fn snapped_logical_radius(radius: i32, scale: Scale<f64>) -> f32 {
    let scale_x = scale.x.abs().max(0.0001);
    (((radius.max(0)) as f64) * scale_x).round().max(0.0) as f32 / scale_x as f32
}

pub fn snapped_logical_rect_for_element(
    rect: LogicalRect,
    element_origin: Point<i32, Logical>,
    snap_origin: Point<i32, Logical>,
    scale: Scale<f64>,
    mode: RectSnapMode,
) -> SnappedLogicalRect {
    let snapped_global = snapped_logical_rect_relative_with_mode(rect, snap_origin, scale, mode);
    SnappedLogicalRect {
        x: snapped_global.x - (element_origin.x - snap_origin.x) as f32,
        y: snapped_global.y - (element_origin.y - snap_origin.y) as f32,
        width: snapped_global.width,
        height: snapped_global.height,
    }
}

pub fn snapped_precise_logical_rect_in_element_space(
    rect: PreciseLogicalRect,
    element_rect: PreciseLogicalRect,
    scale: Scale<f64>,
) -> SnappedLogicalRect {
    let scale_x = scale.x.abs().max(0.0001) as f32;
    let scale_y = scale.y.abs().max(0.0001) as f32;

    let element_left_px = (element_rect.x * scale_x).round();
    let element_top_px = (element_rect.y * scale_y).round();
    let snapped_left_px = (rect.x * scale_x).round();
    let snapped_top_px = (rect.y * scale_y).round();
    let snapped_right_px = ((rect.x + rect.width) * scale_x).round();
    let snapped_bottom_px = ((rect.y + rect.height) * scale_y).round();

    let local_left_px = snapped_left_px - element_left_px;
    let local_top_px = snapped_top_px - element_top_px;
    let local_width_px = (snapped_right_px - snapped_left_px).max(0.0);
    let local_height_px = (snapped_bottom_px - snapped_top_px).max(0.0);

    let element_width_px = ((element_rect.width * scale_x).round()).max(1.0);
    let element_height_px = ((element_rect.height * scale_y).round()).max(1.0);

    SnappedLogicalRect {
        x: local_left_px * element_rect.width.max(0.0001) / element_width_px,
        y: local_top_px * element_rect.height.max(0.0001) / element_height_px,
        width: local_width_px * element_rect.width.max(0.0001) / element_width_px,
        height: local_height_px * element_rect.height.max(0.0001) / element_height_px,
    }
}

pub fn snapped_precise_logical_rect_for_element(
    rect: PreciseLogicalRect,
    element_rect: PreciseLogicalRect,
    snap_origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> SnappedLogicalRect {
    let scale_x = scale.x.abs().max(0.0001) as f32;
    let scale_y = scale.y.abs().max(0.0001) as f32;

    let element_left_px = ((element_rect.x - snap_origin.x as f32) * scale_x).round();
    let element_top_px = ((element_rect.y - snap_origin.y as f32) * scale_y).round();
    let element_right_px =
        (((element_rect.x + element_rect.width) - snap_origin.x as f32) * scale_x).round();
    let element_bottom_px =
        (((element_rect.y + element_rect.height) - snap_origin.y as f32) * scale_y).round();

    let snapped_left_px = ((rect.x - snap_origin.x as f32) * scale_x).round();
    let snapped_top_px = ((rect.y - snap_origin.y as f32) * scale_y).round();
    let snapped_right_px = (((rect.x + rect.width) - snap_origin.x as f32) * scale_x).round();
    let snapped_bottom_px = (((rect.y + rect.height) - snap_origin.y as f32) * scale_y).round();

    let local_left_px = snapped_left_px - element_left_px;
    let local_top_px = snapped_top_px - element_top_px;
    let local_width_px = (snapped_right_px - snapped_left_px).max(0.0);
    let local_height_px = (snapped_bottom_px - snapped_top_px).max(0.0);

    let element_width_px = (element_right_px - element_left_px).max(1.0);
    let element_height_px = (element_bottom_px - element_top_px).max(1.0);

    SnappedLogicalRect {
        x: local_left_px * element_rect.width.max(0.0001) / element_width_px,
        y: local_top_px * element_rect.height.max(0.0001) / element_height_px,
        width: local_width_px * element_rect.width.max(0.0001) / element_width_px,
        height: local_height_px * element_rect.height.max(0.0001) / element_height_px,
    }
}

pub fn snapped_precise_logical_rect_in_area_space(
    rect: PreciseLogicalRect,
    element_rect: PreciseLogicalRect,
    area_width: i32,
    area_height: i32,
    snap_origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> SnappedLogicalRect {
    let scale_x = scale.x.abs().max(0.0001) as f32;
    let scale_y = scale.y.abs().max(0.0001) as f32;
    let origin_x = snap_origin.x as f32;
    let origin_y = snap_origin.y as f32;

    let element_left_px = ((element_rect.x - origin_x) * scale_x).round();
    let element_top_px = ((element_rect.y - origin_y) * scale_y).round();
    let element_right_px = ((element_rect.x + element_rect.width - origin_x) * scale_x).round();
    let element_bottom_px = ((element_rect.y + element_rect.height - origin_y) * scale_y).round();

    let clip_left_px = ((rect.x - origin_x) * scale_x).round();
    let clip_top_px = ((rect.y - origin_y) * scale_y).round();
    let clip_right_px = ((rect.x + rect.width - origin_x) * scale_x).round();
    let clip_bottom_px = ((rect.y + rect.height - origin_y) * scale_y).round();

    let element_width_px = (element_right_px - element_left_px).max(1.0);
    let element_height_px = (element_bottom_px - element_top_px).max(1.0);
    let area_width = area_width.max(1) as f32;
    let area_height = area_height.max(1) as f32;

    SnappedLogicalRect {
        x: (clip_left_px - element_left_px) * area_width / element_width_px,
        y: (clip_top_px - element_top_px) * area_height / element_height_px,
        width: (clip_right_px - clip_left_px).max(0.0) * area_width / element_width_px,
        height: (clip_bottom_px - clip_top_px).max(0.0) * area_height / element_height_px,
    }
}

pub fn snapped_precise_logical_rect_in_root_frame_area_space(
    rect: PreciseLogicalRect,
    element_rect: PreciseLogicalRect,
    area_width: i32,
    area_height: i32,
    root_rect: LogicalRect,
    root_subpixel: RootSubpixelEdges,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> SnappedLogicalRect {
    let element_physical = relative_physical_rect_from_root_precise(
        element_rect,
        root_rect,
        root_subpixel,
        output_geo,
        scale,
    );
    let clip_physical =
        relative_physical_rect_from_root_precise(rect, root_rect, root_subpixel, output_geo, scale);
    let element_width_px = element_physical.size.w.max(1) as f32;
    let element_height_px = element_physical.size.h.max(1) as f32;
    let area_width = area_width.max(1) as f32;
    let area_height = area_height.max(1) as f32;

    SnappedLogicalRect {
        x: (clip_physical.loc.x - element_physical.loc.x) as f32 * area_width / element_width_px,
        y: (clip_physical.loc.y - element_physical.loc.y) as f32 * area_height / element_height_px,
        width: clip_physical.size.w.max(0) as f32 * area_width / element_width_px,
        height: clip_physical.size.h.max(0) as f32 * area_height / element_height_px,
    }
}

pub fn logical_rect_to_physical_buffer_rect_f64(
    rect: LogicalRect,
    origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> Rectangle<f64, Buffer> {
    let scale_x = scale.x.abs().max(0.0001);
    let scale_y = scale.y.abs().max(0.0001);
    let left = ((rect.x - origin.x) as f64) * scale_x;
    let top = ((rect.y - origin.y) as f64) * scale_y;
    let right = (((rect.x + rect.width) - origin.x) as f64) * scale_x;
    let bottom = (((rect.y + rect.height) - origin.y) as f64) * scale_y;
    Rectangle::new(
        Point::from((left, top)),
        ((right - left).max(0.0), (bottom - top).max(0.0)).into(),
    )
}

pub fn precise_logical_rect_to_physical_buffer_rect(
    rect: PreciseLogicalRect,
    origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> Rectangle<f64, Buffer> {
    let scale_x = scale.x.abs().max(0.0001) as f32;
    let scale_y = scale.y.abs().max(0.0001) as f32;
    let origin_x = origin.x as f32;
    let origin_y = origin.y as f32;
    let left = (rect.x - origin_x) * scale_x;
    let top = (rect.y - origin_y) * scale_y;
    let right = (rect.x + rect.width - origin_x) * scale_x;
    let bottom = (rect.y + rect.height - origin_y) * scale_y;
    Rectangle::new(
        Point::from((left as f64, top as f64)),
        (
            (right - left).max(0.0) as f64,
            (bottom - top).max(0.0) as f64,
        )
            .into(),
    )
}

pub fn logical_rect_to_physical_rect(
    rect: LogicalRect,
    origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    let scale_x = scale.x.abs().max(0.0001);
    let scale_y = scale.y.abs().max(0.0001);
    let left = (((rect.x - origin.x) as f64) * scale_x).round() as i32;
    let top = (((rect.y - origin.y) as f64) * scale_y).round() as i32;
    let right = ((((rect.x + rect.width) - origin.x) as f64) * scale_x).round() as i32;
    let bottom = ((((rect.y + rect.height) - origin.y) as f64) * scale_y).round() as i32;
    Rectangle::new(
        Point::from((left, top)),
        ((right - left).max(0), (bottom - top).max(0)).into(),
    )
}

pub fn logical_point_to_physical_point_global_edges(
    point: Point<i32, Logical>,
    origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    let scale_x = scale.x.abs().max(0.0001);
    let scale_y = scale.y.abs().max(0.0001);
    let point_x = ((point.x as f64) * scale_x).round() as i32;
    let point_y = ((point.y as f64) * scale_y).round() as i32;
    let origin_x = ((origin.x as f64) * scale_x).round() as i32;
    let origin_y = ((origin.y as f64) * scale_y).round() as i32;
    Point::from((point_x - origin_x, point_y - origin_y))
}

pub fn precise_logical_point_to_physical_point_global_edges(
    point: Point<f64, Logical>,
    origin: Point<i32, Logical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    let scale_x = scale.x.abs().max(0.0001);
    let scale_y = scale.y.abs().max(0.0001);
    let point_x = (point.x * scale_x).round() as i32;
    let point_y = (point.y * scale_y).round() as i32;
    let origin_x = ((origin.x as f64) * scale_x).round() as i32;
    let origin_y = ((origin.y as f64) * scale_y).round() as i32;
    Point::from((point_x - origin_x, point_y - origin_y))
}

pub fn logical_point_to_relative_physical_point_from_output(
    point: Point<i32, Logical>,
    output_geo: Rectangle<i32, Logical>,
    capture_origin_physical: Point<i32, Physical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    logical_point_to_relative_physical_point_from_origin(
        point,
        output_geo.loc,
        capture_origin_physical,
        scale,
    )
}

pub fn logical_point_to_relative_physical_point_from_origin(
    point: Point<i32, Logical>,
    output_origin: Point<i32, Logical>,
    capture_origin_physical: Point<i32, Physical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    let global = logical_point_to_physical_point_global_edges(point, output_origin, scale);
    Point::from((
        global.x - capture_origin_physical.x,
        global.y - capture_origin_physical.y,
    ))
}

pub fn precise_logical_point_to_relative_physical_point_from_output(
    point: Point<f64, Logical>,
    output_geo: Rectangle<i32, Logical>,
    capture_origin_physical: Point<i32, Physical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    precise_logical_point_to_relative_physical_point_from_origin(
        point,
        output_geo.loc,
        capture_origin_physical,
        scale,
    )
}

pub fn precise_logical_point_to_relative_physical_point_from_origin(
    point: Point<f64, Logical>,
    output_origin: Point<i32, Logical>,
    capture_origin_physical: Point<i32, Physical>,
    scale: Scale<f64>,
) -> Point<i32, Physical> {
    let global = precise_logical_point_to_physical_point_global_edges(point, output_origin, scale);
    Point::from((
        global.x - capture_origin_physical.x,
        global.y - capture_origin_physical.y,
    ))
}

pub fn logical_size_to_physical_buffer_size(
    width: i32,
    height: i32,
    scale: Scale<f64>,
) -> (i32, i32) {
    let scale_x = scale.x.abs().max(0.0001);
    let scale_y = scale.y.abs().max(0.0001);
    (
        ((width as f64) * scale_x).round().max(0.0) as i32,
        ((height as f64) * scale_y).round().max(0.0) as i32,
    )
}

#[derive(Debug)]
pub struct AlphaRenderElement<E> {
    element: E,
    alpha: f32,
}

impl<E> AlphaRenderElement<E> {
    pub fn from_element(element: E, alpha: f32) -> Self {
        Self {
            element,
            alpha: alpha.clamp(0.0, 1.0),
        }
    }
}

impl<E: Element> Element for AlphaRenderElement<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.element.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.element.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.element.location(scale)
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.element.damage_since(scale, commit)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.element.alpha() * self.alpha
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }

    fn is_framebuffer_effect(&self) -> bool {
        self.element.is_framebuffer_effect()
    }

    fn framebuffer_capture_region(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.element.framebuffer_capture_region(scale)
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for AlphaRenderElement<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        self.element
            .draw(frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.element.underlying_storage(renderer)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        self.element.capture_framebuffer(frame, src, dst, cache)
    }
}

/// Wraps a render element and discards its claimed opaque regions.
///
/// Applied when `COMPOSITOR.rendering.surfacePolicy` resolves
/// `opaqueRegion: "ignore"` for a surface: the damage tracker then neither
/// culls elements composited behind this one nor renders any of its pixels
/// with blending disabled. Used for clients that over-declare their opaque
/// region (e.g. GTK3 tooltips claim the full rect despite transparent
/// rounded corners).
#[derive(Debug)]
pub struct IgnoredOpaqueRegionElement<E> {
    element: E,
}

impl<E> IgnoredOpaqueRegionElement<E> {
    pub fn from_element(element: E) -> Self {
        Self { element }
    }
}

impl<E: Element> Element for IgnoredOpaqueRegionElement<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.element.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.element.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.element.location(scale)
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.element.damage_since(scale, commit)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.element.alpha()
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }

    fn is_framebuffer_effect(&self) -> bool {
        self.element.is_framebuffer_effect()
    }

    fn framebuffer_capture_region(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.element.framebuffer_capture_region(scale)
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for IgnoredOpaqueRegionElement<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        self.element
            .draw(frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.element.underlying_storage(renderer)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        self.element.capture_framebuffer(frame, src, dst, cache)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WindowVisualState {
    pub origin: Point<i32, Physical>,
    pub scale: Scale<f64>,
    pub translation: Point<i32, Physical>,
    pub opacity: f32,
}

/// A window's snapshot under its visual transform: scaled about `origin`,
/// then moved by `translation`.
pub fn transformed_snapshot_element<E: Element>(
    element: E,
    visual: WindowVisualState,
) -> RelocateRenderElement<RescaleRenderElement<E>> {
    RelocateRenderElement::from_element(
        RescaleRenderElement::from_element(element, visual.origin, visual.scale),
        visual.translation,
        Relocate::Relative,
    )
}

/// Where `rect`, in output physical pixels, lands once the window's visual
/// transform is applied to it as `transformed_snapshot_element` applies it to
/// the window's snapshot: scaled about `origin`, then moved by `translation`.
pub fn visually_transformed_physical_rect(
    rect: Rectangle<i32, Physical>,
    visual: WindowVisualState,
) -> Rectangle<f64, Physical> {
    let map = |at: i32, origin: i32, scale: f64, translation: i32| {
        origin as f64 + (at - origin) as f64 * scale + translation as f64
    };
    Rectangle::new(
        Point::from((
            map(
                rect.loc.x,
                visual.origin.x,
                visual.scale.x,
                visual.translation.x,
            ),
            map(
                rect.loc.y,
                visual.origin.y,
                visual.scale.y,
                visual.translation.y,
            ),
        )),
        (
            rect.size.w as f64 * visual.scale.x,
            rect.size.h as f64 * visual.scale.y,
        )
            .into(),
    )
}

pub fn requires_full_window_snapshot(visual: WindowVisualState) -> bool {
    // Translation and opacity can be applied directly to each render element. Scaling a complete
    // window element-by-element can expose subpixel rounding differences between its surfaces and
    // decoration nodes, so keep the single-snapshot path for scale animations.
    const VISUAL_IDENTITY_EPSILON: f64 = 1e-3;
    (visual.scale.x - 1.0).abs() >= VISUAL_IDENTITY_EPSILON
        || (visual.scale.y - 1.0).abs() >= VISUAL_IDENTITY_EPSILON
}

pub fn is_identity_visual_geometry(visual: WindowVisualState) -> bool {
    // Animation values pass through runtime serialization before reaching renderer space. Use a
    // small tolerance so a visually finished scale animation cannot leave a window on the more
    // expensive transform path indefinitely. Opacity is deliberately excluded: normal rendering
    // applies it directly to each element and does not require a full-window transform snapshot.
    visual.translation.x == 0 && visual.translation.y == 0 && !requires_full_window_snapshot(visual)
}

pub fn window_visual_state(
    rect: LogicalRect,
    transform: WindowTransform,
    output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> WindowVisualState {
    let logical_origin = Point::<f64, Logical>::from((
        rect.x as f64 + rect.width as f64 * transform.origin.x,
        rect.y as f64 + rect.height as f64 * transform.origin.y,
    ));
    let origin = (logical_origin - output_geo.loc.to_f64()).to_physical_precise_round(output_scale);
    let translation = Point::<f64, Logical>::from((transform.translate_x, transform.translate_y))
        .to_physical_precise_round(output_scale);

    WindowVisualState {
        origin,
        scale: Scale::from((transform.scale_x.max(0.0), transform.scale_y.max(0.0))),
        translation,
        opacity: transform.opacity,
    }
}

pub fn root_physical_origin(
    rect: LogicalRect,
    output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> Point<i32, Physical> {
    Point::<f64, Logical>::from((
        (rect.x - output_geo.loc.x) as f64,
        (rect.y - output_geo.loc.y) as f64,
    ))
    .to_physical_precise_round(output_scale)
}

/// Sub-logical-pixel remainders of the four managed-rect edges after the
/// integer quantization (`edge - round(edge)`, each in (-0.5, 0.5]). The
/// integer layout pipeline is unaware of these fractions; rendering re-applies
/// them when converting the root rect to physical pixels, so rect animations
/// move *and resize* at physical-pixel granularity instead of logical.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RootSubpixelEdges {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

/// `root_physical_origin` with the sub-logical-pixel remainder of the managed
/// rect applied before the physical rounding. The integer layout pipeline is
/// unaware of the fraction, so shifting only this anchor moves the whole
/// rendered window rigidly at physical-pixel granularity.
pub fn root_physical_origin_precise(
    rect: LogicalRect,
    subpixel: RootSubpixelEdges,
    output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> Point<i32, Physical> {
    Point::<f64, Logical>::from((
        (rect.x - output_geo.loc.x) as f64 + subpixel.left,
        (rect.y - output_geo.loc.y) as f64 + subpixel.top,
    ))
    .to_physical_precise_round(output_scale)
}

fn relative_physical_rect_in_root_frame(
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    root_rect: LogicalRect,
    _root_subpixel: RootSubpixelEdges,
    _output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    // SSD layout already places every node edge on the physical pixel grid,
    // relative to the root origin (`root.x + px / scale`). Mapping back is a
    // plain scale-and-round in root-local space, which recovers those exact
    // pixels: no proportional re-fitting, no second quantization grid. The
    // window's global placement (including its sub-logical-pixel offset)
    // enters only through `root_physical_origin_precise`.
    let scale_x = output_scale.x.abs().max(0.0001);
    let scale_y = output_scale.y.abs().max(0.0001);
    let root_x = root_rect.x as f64;
    let root_y = root_rect.y as f64;
    let to_px_x = |x: f32| round_half_up((x as f64 - root_x) * scale_x);
    let to_px_y = |y: f32| round_half_up((y as f64 - root_y) * scale_y);

    let left_px = to_px_x(left);
    let top_px = to_px_y(top);
    let right_px = to_px_x(right);
    let bottom_px = to_px_y(bottom);
    Rectangle::new(
        Point::from((left_px, top_px)),
        ((right_px - left_px).max(0), (bottom_px - top_px).max(0)).into(),
    )
}

fn round_half_up(value: f64) -> i32 {
    crate::ssd::round_half_up(value)
}

pub fn relative_physical_rect_from_root(
    rect: LogicalRect,
    root_rect: LogicalRect,
    root_subpixel: RootSubpixelEdges,
    output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
    shared_rect: Option<LogicalRect>,
) -> Rectangle<i32, Physical> {
    let _ = shared_rect;
    relative_physical_rect_in_root_frame(
        rect.x as f32,
        rect.y as f32,
        (rect.x + rect.width) as f32,
        (rect.y + rect.height) as f32,
        root_rect,
        root_subpixel,
        output_geo,
        output_scale,
    )
}

pub fn relative_physical_rect_from_root_snapped_edges(
    rect: LogicalRect,
    root_rect: LogicalRect,
    root_subpixel: RootSubpixelEdges,
    output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    relative_physical_rect_from_root_precise(
        PreciseLogicalRect {
            x: rect.x as f32,
            y: rect.y as f32,
            width: rect.width as f32,
            height: rect.height as f32,
        },
        root_rect,
        root_subpixel,
        output_geo,
        output_scale,
    )
}

pub fn snapped_logical_rect_from_relative_physical(
    rect: Rectangle<i32, Physical>,
    scale: Scale<f64>,
) -> SnappedLogicalRect {
    let scale_x = scale.x.abs().max(0.0001) as f32;
    let scale_y = scale.y.abs().max(0.0001) as f32;
    SnappedLogicalRect {
        x: rect.loc.x as f32 / scale_x,
        y: rect.loc.y as f32 / scale_y,
        width: rect.size.w as f32 / scale_x,
        height: rect.size.h as f32 / scale_y,
    }
}

pub fn relative_physical_rect_from_root_precise(
    rect: PreciseLogicalRect,
    root_rect: LogicalRect,
    root_subpixel: RootSubpixelEdges,
    output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    relative_physical_rect_in_root_frame(
        rect.x,
        rect.y,
        rect.x + rect.width,
        rect.y + rect.height,
        root_rect,
        root_subpixel,
        output_geo,
        output_scale,
    )
}

// The `_global_*` variants below historically snapped node edges in
// output-global space. That made every result depend on the window's position
// phase (`x × scale mod 1`), so backdrop geometry and fallback rects wobbled
// by ±1px as the window moved at fractional scales. They now snap in
// root-local space: subtract the root origin in logical space first, then
// scale and round. Results are position-independent; the window's global
// placement enters exactly once, via `root_physical_origin*`.

pub fn relative_physical_rect_from_root_global_origin_size(
    rect: LogicalRect,
    root_rect: LogicalRect,
    _output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    let scale_x = output_scale.x.abs().max(0.0001);
    let scale_y = output_scale.y.abs().max(0.0001);
    let left_px = (((rect.x - root_rect.x) as f64) * scale_x).round() as i32;
    let top_px = (((rect.y - root_rect.y) as f64) * scale_y).round() as i32;
    let width_px = ((rect.width as f64) * scale_x).round().max(0.0) as i32;
    let height_px = ((rect.height as f64) * scale_y).round().max(0.0) as i32;
    Rectangle::new(Point::from((left_px, top_px)), (width_px, height_px).into())
}

pub fn relative_physical_rect_from_root_global_edges_precise(
    rect: PreciseLogicalRect,
    root_rect: LogicalRect,
    _output_geo: Rectangle<i32, Logical>,
    output_scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    let scale_x = output_scale.x.abs().max(0.0001) as f32;
    let scale_y = output_scale.y.abs().max(0.0001) as f32;
    let root_x = root_rect.x as f32;
    let root_y = root_rect.y as f32;
    let left_px = (((rect.x - root_x) * scale_x).round()) as i32;
    let top_px = (((rect.y - root_y) * scale_y).round()) as i32;
    let right_px = ((((rect.x + rect.width) - root_x) * scale_x).round()) as i32;
    let bottom_px = ((((rect.y + rect.height) - root_y) * scale_y).round()) as i32;
    Rectangle::new(
        Point::from((left_px, top_px)),
        ((right_px - left_px).max(0), (bottom_px - top_px).max(0)).into(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        PreciseLogicalRect, RootSubpixelEdges, WindowVisualState, backdrop_sample_rects,
        is_identity_visual_geometry, logical_size_to_physical_buffer_size,
        relative_physical_rect_from_root, relative_physical_rect_from_root_snapped_edges,
        requires_full_window_snapshot, root_physical_origin_precise,
        snapped_precise_logical_rect_in_area_space, snapped_precise_logical_rect_in_element_space,
        snapped_precise_logical_rect_in_root_frame_area_space, transformed_snapshot_element,
        window_backdrop_geometry, window_visual_state,
    };
    use crate::ssd::{LogicalRect, WindowTransform};
    use smithay::utils::{Logical, Physical, Point, Rectangle, Scale};

    /// formalwm lays a window out at the size it gave the client and scales
    /// it to the box it animates through: a column going from a third of the
    /// output to all of it starts out three times as wide as its rect.
    #[test]
    fn backdrop_is_sampled_where_a_scaled_window_is_drawn() {
        let root = LogicalRect::new(10, 10, 336, 1260);
        let node = LogicalRect::new(12, 12, 332, 1256);
        let mut transform = WindowTransform::default();
        transform.origin.x = 0.0;
        transform.origin.y = 0.0;
        transform.scale_x = 3.0;
        let (sampled, precise) = backdrop_sample_rects(node, None, root, transform);
        assert_eq!(
            (sampled.x, sampled.y, sampled.width, sampled.height),
            (16, 12, 996, 1256)
        );
        assert_eq!(
            (precise.x, precise.y, precise.width, precise.height),
            (16.0, 12.0, 996.0, 1256.0)
        );

        let precise_node = PreciseLogicalRect {
            x: 12.5,
            y: 12.0,
            width: 331.5,
            height: 1256.0,
        };
        let (_, precise) = backdrop_sample_rects(node, Some(precise_node), root, transform);
        assert_eq!((precise.x, precise.width), (17.5, 994.5));
    }

    const OUTPUT_SIZE: (i32, i32) = (2048, 1280);

    fn output() -> Rectangle<i32, Logical> {
        Rectangle::from_size(OUTPUT_SIZE.into())
    }

    fn scaled_by(scale_x: f64, scale_y: f64) -> WindowTransform {
        let mut transform = WindowTransform::default();
        transform.origin.x = 0.0;
        transform.origin.y = 0.0;
        transform.scale_x = scale_x;
        transform.scale_y = scale_y;
        transform
    }

    /// A column laid out a third of the output wide and drawn three times as
    /// wide by its snapshot (formalwm widening it with SUPER+R), and a window
    /// zoomed out to 0.3 by the overview: its backdrop is sampled where the
    /// window's snapshot element, scaled and moved as the backends move it,
    /// puts the backdrop node, and the capture holds that.
    #[test]
    fn a_scaled_windows_backdrop_is_sampled_where_its_snapshot_draws_it() {
        use smithay::backend::renderer::{
            element::{Element, Id, Kind, solid::SolidColorRenderElement},
            utils::CommitCounter,
        };
        let scale = Scale::from(1.25);
        for (root, node, transform) in [
            (
                LogicalRect::new(10, 10, 336, 1260),
                LogicalRect::new(12, 12, 332, 1256),
                scaled_by(3.0, 1.0),
            ),
            (
                LogicalRect::new(700, 10, 1008, 1260),
                LogicalRect::new(702, 40, 1004, 1226),
                scaled_by(0.3, 0.3),
            ),
        ] {
            let backdrop = window_backdrop_geometry(
                node,
                None,
                root,
                RootSubpixelEdges::default(),
                transform,
                24,
                None,
                output(),
                scale,
            );
            let root_origin =
                root_physical_origin_precise(root, RootSubpixelEdges::default(), output(), scale);
            let laid_out = SolidColorRenderElement::new(
                Id::new(),
                Rectangle::new(root_origin + backdrop.geometry.loc, backdrop.geometry.size),
                CommitCounter::default(),
                [0.0, 0.0, 0.0, 1.0],
                Kind::Unspecified,
            );
            let visual = window_visual_state(root, transform, output(), scale);
            let drawn = transformed_snapshot_element(laid_out, visual).geometry(scale);
            let sampled = backdrop.sample_region;
            let at = |physical: i32, origin: i32, sampled: f64| {
                ((physical - origin) as f64 - sampled).abs()
            };
            assert!(
                at(drawn.loc.x, backdrop.capture_origin.x, sampled.loc.x) <= 1.0,
                "{drawn:?} {backdrop:?}"
            );
            assert!(
                at(drawn.loc.y, backdrop.capture_origin.y, sampled.loc.y) <= 1.0,
                "{drawn:?} {backdrop:?}"
            );
            assert!(
                (drawn.size.w as f64 - sampled.size.w).abs() <= 1.0,
                "{drawn:?} {backdrop:?}"
            );
            assert!(
                (drawn.size.h as f64 - sampled.size.h).abs() <= 1.0,
                "{drawn:?} {backdrop:?}"
            );

            let (w, h) = logical_size_to_physical_buffer_size(
                backdrop.capture.size.w,
                backdrop.capture.size.h,
                scale,
            );
            assert!(sampled.loc.x >= 0.0 && sampled.loc.y >= 0.0, "{backdrop:?}");
            assert!(
                sampled.loc.x + sampled.size.w <= w as f64 + 0.5,
                "{backdrop:?}"
            );
            assert!(
                sampled.loc.y + sampled.size.h <= h as f64 + 0.5,
                "{backdrop:?}"
            );
        }
    }

    fn animate(
        frames: impl Iterator<Item = f64>,
        start: Option<Rectangle<i32, Logical>>,
    ) -> Vec<Rectangle<i32, Logical>> {
        let root = LogicalRect::new(700, 10, 1008, 1260);
        let node = LogicalRect::new(702, 12, 1004, 1256);
        let mut previous = start;
        frames
            .map(|scale| {
                let capture = window_backdrop_geometry(
                    node,
                    None,
                    root,
                    RootSubpixelEdges::default(),
                    scaled_by(scale, scale),
                    24,
                    previous,
                    output(),
                    Scale::from(1.25),
                )
                .capture;
                previous = Some(capture);
                capture
            })
            .collect()
    }

    /// The overview zooming a window out from rest to 0.3 over 60 frames: one
    /// capture region for the whole animation, so one capture size and one
    /// blur pyramid.
    #[test]
    fn a_window_scaling_down_keeps_one_capture_region() {
        let captures = animate((0..=60).map(|frame| 1.0 - 0.7 * frame as f64 / 60.0), None);
        assert!(
            captures.iter().all(|capture| *capture == captures[0]),
            "{captures:?}"
        );
        assert_eq!(
            captures[0],
            Rectangle::new((678, 0).into(), (1052, 1280).into())
        );
    }

    /// A column widened from a third of its width with no region to start
    /// from: the first animated frame's region already reaches the rest rect,
    /// so the whole animation shares it, and the window at rest goes back to
    /// the tight one.
    #[test]
    fn a_widening_column_shares_one_capture_region_until_it_rests() {
        let captures = animate((0..=60).map(|frame| 0.3 + 0.7 * frame as f64 / 60.0), None);
        let mut regions = captures.clone();
        regions.dedup();
        assert!(regions.len() <= 2, "{regions:?}");
        let at_rest = animate(std::iter::once(1.0), None)[0];
        assert_eq!(captures.last(), Some(&at_rest));
    }

    #[test]
    fn backdrop_of_a_window_at_rest_is_sampled_at_its_rect() {
        let root = LogicalRect::new(10, 10, 1008, 1260);
        let node = LogicalRect::new(12, 12, 1004, 1256);
        let (sampled, _) = backdrop_sample_rects(node, None, root, WindowTransform::default());
        assert_eq!(
            (sampled.x, sampled.y, sampled.width, sampled.height),
            (12, 12, 1004, 1256)
        );
    }

    #[test]
    fn opacity_only_visual_state_has_identity_geometry() {
        let visual = WindowVisualState {
            origin: Point::<i32, Physical>::from((10, 20)),
            scale: Scale::from((1.0, 1.0)),
            translation: Point::from((0, 0)),
            opacity: 0.5,
        };

        assert!(is_identity_visual_geometry(visual));
    }

    #[test]
    fn translated_visual_state_does_not_have_identity_geometry() {
        let visual = WindowVisualState {
            origin: Point::<i32, Physical>::from((10, 20)),
            scale: Scale::from((1.0, 1.0)),
            translation: Point::from((1, 0)),
            opacity: 1.0,
        };

        assert!(!is_identity_visual_geometry(visual));
    }

    #[test]
    fn translated_visual_state_does_not_require_full_window_snapshot() {
        let visual = WindowVisualState {
            origin: Point::<i32, Physical>::from((10, 20)),
            scale: Scale::from((1.0, 1.0)),
            translation: Point::from((1, 0)),
            opacity: 0.5,
        };

        assert!(!requires_full_window_snapshot(visual));
    }

    #[test]
    fn scaled_visual_state_requires_full_window_snapshot() {
        let visual = WindowVisualState {
            origin: Point::<i32, Physical>::from((10, 20)),
            scale: Scale::from((0.95, 1.0)),
            translation: Point::from((0, 0)),
            opacity: 1.0,
        };

        assert!(requires_full_window_snapshot(visual));
    }

    #[test]
    fn snapped_edge_relative_rect_is_translation_stable_against_output() {
        let output_geo = Rectangle::<i32, Logical>::new((0, 0).into(), (4000, 2000).into());
        let scale = Scale::from((1.6, 1.6));

        let root_a = LogicalRect::new(100, 40, 200, 80);
        let child_a = LogicalRect::new(111, 40, 10, 10);
        let root_b = LogicalRect::new(101, 40, 200, 80);
        let child_b = LogicalRect::new(112, 40, 10, 10);

        let subpixel = super::RootSubpixelEdges::default();
        let snapped_a = relative_physical_rect_from_root_snapped_edges(
            child_a, root_a, subpixel, output_geo, scale,
        );
        let snapped_b = relative_physical_rect_from_root_snapped_edges(
            child_b, root_b, subpixel, output_geo, scale,
        );
        let local_a =
            relative_physical_rect_from_root(child_a, root_a, subpixel, output_geo, scale, None);
        let local_b =
            relative_physical_rect_from_root(child_b, root_b, subpixel, output_geo, scale, None);

        assert_eq!(snapped_a, snapped_b);
        assert_eq!(local_a, local_b);
    }

    #[test]
    fn root_frame_geometry_is_position_independent() {
        let output_geo = Rectangle::<i32, Logical>::new((0, 0).into(), (4000, 2000).into());
        let scale = Scale::from((1.8, 1.8));
        let subpixel = super::RootSubpixelEdges::default();
        // width × scale is fractional (401 × 1.8 = 721.8, 233 × 1.8 = 419.4) —
        // the case where output-global edge snapping made the frame size (and
        // with it every frame-mapped descendant) depend on the window position.
        let base = LogicalRect::new(100, 40, 401, 233);
        let child = PreciseLogicalRect {
            x: 130.0,
            y: 70.0,
            width: 30.5,
            height: 17.25,
        };
        let reference = super::relative_physical_rect_from_root_precise(
            child, base, subpixel, output_geo, scale,
        );

        for delta in 1..8 {
            let root = LogicalRect::new(base.x + delta, base.y + delta, base.width, base.height);
            let moved_child = PreciseLogicalRect {
                x: child.x + delta as f32,
                y: child.y + delta as f32,
                width: child.width,
                height: child.height,
            };
            assert_eq!(
                super::relative_physical_rect_from_root_precise(
                    moved_child,
                    root,
                    subpixel,
                    output_geo,
                    scale
                ),
                reference,
                "frame-mapped geometry must not change when the window moves (delta {delta})"
            );
        }
    }

    #[test]
    fn root_frame_uses_output_snapped_far_edges() {
        let output_geo = Rectangle::<i32, Logical>::new((0, 0).into(), (4000, 2000).into());
        let scale = Scale::from((1.25, 1.25));
        let root = LogicalRect::new(1, 0, 10, 10);

        let full = relative_physical_rect_from_root_snapped_edges(
            root,
            root,
            super::RootSubpixelEdges::default(),
            output_geo,
            scale,
        );

        assert_eq!(full.loc.x, 0);
        assert_eq!(full.loc.y, 0);
        assert_eq!(full.size.w, 13);
        assert_eq!(full.size.h, 13);
    }

    #[test]
    fn precise_clip_in_element_space_is_translation_stable() {
        let scale = Scale::from((1.6, 1.6));
        let element_a = PreciseLogicalRect {
            x: 100.0,
            y: 40.0,
            width: 18.75,
            height: 18.75,
        };
        let clip_a = PreciseLogicalRect {
            x: 101.875,
            y: 41.875,
            width: 15.0,
            height: 15.0,
        };
        let element_b = PreciseLogicalRect {
            x: 101.0,
            y: 40.0,
            width: 18.75,
            height: 18.75,
        };
        let clip_b = PreciseLogicalRect {
            x: 102.875,
            y: 41.875,
            width: 15.0,
            height: 15.0,
        };

        assert_eq!(
            snapped_precise_logical_rect_in_element_space(clip_a, element_a, scale),
            snapped_precise_logical_rect_in_element_space(clip_b, element_b, scale)
        );
    }

    #[test]
    fn precise_clip_in_area_space_matches_shader_area_at_fractional_scale() {
        let scale = Scale::from((1.6, 1.6));
        let element = PreciseLogicalRect {
            x: 3.75,
            y: 3.75,
            width: 900.0,
            height: 30.0,
        };

        let clip = snapped_precise_logical_rect_in_area_space(
            element,
            element,
            900,
            30,
            Point::from((0, 0)),
            scale,
        );

        assert_eq!(clip.x, 0.0);
        assert_eq!(clip.y, 0.0);
        assert_eq!(clip.width, 900.0);
        assert_eq!(clip.height, 30.0);
    }

    #[test]
    fn root_frame_area_clip_matches_root_frame_geometry() {
        let output_geo = Rectangle::<i32, Logical>::new((0, 0).into(), (4000, 2000).into());
        let scale = Scale::from((1.25, 1.25));
        let root = LogicalRect::new(1, 0, 10, 10);
        let element = PreciseLogicalRect {
            x: 1.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };

        let clip = snapped_precise_logical_rect_in_root_frame_area_space(
            element,
            element,
            10,
            10,
            root,
            super::RootSubpixelEdges::default(),
            output_geo,
            scale,
        );

        assert_eq!(clip.x, 0.0);
        assert_eq!(clip.y, 0.0);
        assert_eq!(clip.width, 10.0);
        assert_eq!(clip.height, 10.0);
    }
}

pub fn transformed_root_rect(rect: LogicalRect, transform: WindowTransform) -> LogicalRect {
    transformed_rect(rect, rect, transform)
}

pub fn transformed_rect(
    rect: LogicalRect,
    reference_rect: LogicalRect,
    transform: WindowTransform,
) -> LogicalRect {
    let origin_x = reference_rect.x as f64 + reference_rect.width as f64 * transform.origin.x;
    let origin_y = reference_rect.y as f64 + reference_rect.height as f64 * transform.origin.y;

    let left = origin_x + (rect.x as f64 - origin_x) * transform.scale_x + transform.translate_x;
    let top = origin_y + (rect.y as f64 - origin_y) * transform.scale_y + transform.translate_y;
    let rect_right = rect.x.saturating_add(rect.width);
    let rect_bottom = rect.y.saturating_add(rect.height);
    let right =
        origin_x + (rect_right as f64 - origin_x) * transform.scale_x + transform.translate_x;
    let bottom =
        origin_y + (rect_bottom as f64 - origin_y) * transform.scale_y + transform.translate_y;

    let x = left.min(right).floor() as i32;
    let y = top.min(bottom).floor() as i32;
    let width = (left.max(right) - left.min(right)).ceil() as i32;
    let height = (top.max(bottom) - top.min(bottom)).ceil() as i32;

    LogicalRect::new(x, y, width.max(0), height.max(0))
}

/// Where a node's backdrop is sampled: the node's rect under the window's
/// visual transform, the screen behind the node as it is drawn. The node is
/// laid out at its raw rect and moved by the transform afterwards (in a scale
/// animation with the whole window, as one snapshot), so its backdrop is
/// sampled there and drawn back into the raw rect. Sampled at the raw rect
/// instead, a window scaled from the size it was given to the box it animates
/// through shows the backdrop of that size stretched over the box.
pub fn backdrop_sample_rects(
    rect: LogicalRect,
    rect_precise: Option<PreciseLogicalRect>,
    reference_rect: LogicalRect,
    transform: WindowTransform,
) -> (LogicalRect, PreciseLogicalRect) {
    let sampled = transformed_rect(rect, reference_rect, transform);
    let precise = rect_precise
        .map(|precise| transformed_precise_rect(precise, reference_rect, transform))
        .unwrap_or_else(|| precise_rect_from_logical(sampled));
    (sampled, precise)
}

/// The region a window node's backdrop is captured and blurred over, clipped
/// to the output. At rest it is `sampled` plus `padding`, the blur's reach.
/// While the window's transform animates it keeps the previous region as long
/// as that still holds `sampled` plus half the padding, so a window scaling
/// down (or back up to where it started) is sampled from one capture and one
/// blur at one size, at the price of a little less reach at its edges. When
/// the window leaves it, it grows to cover both, `at_rest` (where a resize
/// animation ends) and a quarter of the window each way, so the window
/// outruns it at most a few times per animation.
pub fn backdrop_capture_region(
    sampled: LogicalRect,
    at_rest: LogicalRect,
    padding: i32,
    previous: Option<Rectangle<i32, Logical>>,
    animating: bool,
    output_geo: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let padded = |rect: LogicalRect, padding: i32| {
        Rectangle::<i32, Logical>::new(
            Point::from((rect.x - padding, rect.y - padding)),
            (rect.width + padding * 2, rect.height + padding * 2).into(),
        )
    };
    let clip = |rect: Rectangle<i32, Logical>| rect.intersection(output_geo).unwrap_or(rect);
    let needed = clip(padded(sampled, padding));
    if !animating {
        return needed;
    }
    match previous {
        Some(previous) if previous.contains_rect(clip(padded(sampled, padding / 2))) => previous,
        previous => {
            let reach = Point::<i32, Logical>::from((needed.size.w / 4, needed.size.h / 4));
            let grown = Rectangle::new(
                needed.loc - reach,
                (needed.size.w + reach.x * 2, needed.size.h + reach.y * 2).into(),
            )
            .merge(padded(at_rest, padding));
            clip(previous.map_or(grown, |previous| previous.merge(grown)))
        }
    }
}

/// Where a window node's backdrop comes from and goes, for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowBackdropGeometry {
    /// The node's rect under the transform the window is drawn with.
    pub sampled: LogicalRect,
    /// What is captured and blurred, see [`backdrop_capture_region`].
    pub capture: Rectangle<i32, Logical>,
    /// The capture's top left corner in output physical pixels.
    pub capture_origin: Point<i32, Physical>,
    /// Where the window draws the node, in the capture's pixels.
    pub sample_region: Rectangle<f64, Buffer>,
    /// The node laid out in its window, relative to the window's root, in
    /// physical pixels: the backdrop element's geometry.
    pub geometry: Rectangle<i32, Physical>,
}

/// The backdrop of the node at `rect` in a window laid out at `root_rect`
/// and drawn with `transform`: sampled where the window's snapshot puts the
/// node, drawn back at the node's layout rect, which the snapshot then moves
/// to where it was sampled.
pub fn window_backdrop_geometry(
    rect: LogicalRect,
    rect_precise: Option<PreciseLogicalRect>,
    root_rect: LogicalRect,
    root_subpixel: RootSubpixelEdges,
    transform: WindowTransform,
    padding: i32,
    previous_capture: Option<Rectangle<i32, Logical>>,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> WindowBackdropGeometry {
    let (sampled, _) = backdrop_sample_rects(rect, rect_precise, root_rect, transform);
    let visual = window_visual_state(root_rect, transform, output_geo, scale);
    let capture = backdrop_capture_region(
        sampled,
        rect,
        padding.max(0),
        previous_capture,
        !is_identity_visual_geometry(visual),
        output_geo,
    );
    let capture_origin =
        logical_point_to_physical_point_global_edges(capture.loc, output_geo.loc, scale);
    let geometry = relative_physical_rect_from_root_precise(
        rect_precise.unwrap_or_else(|| precise_rect_from_logical(rect)),
        root_rect,
        root_subpixel,
        output_geo,
        scale,
    );
    let root_origin = root_physical_origin_precise(root_rect, root_subpixel, output_geo, scale);
    let laid_out = Rectangle::new(root_origin + geometry.loc, geometry.size);
    let drawn = visually_transformed_physical_rect(laid_out, visual);
    let sample_region = Rectangle::new(
        Point::from((
            drawn.loc.x - capture_origin.x as f64,
            drawn.loc.y - capture_origin.y as f64,
        )),
        (drawn.size.w, drawn.size.h).into(),
    );
    WindowBackdropGeometry {
        sampled,
        capture,
        capture_origin,
        sample_region,
        geometry,
    }
}

pub fn transformed_precise_rect(
    rect: PreciseLogicalRect,
    reference_rect: LogicalRect,
    transform: WindowTransform,
) -> PreciseLogicalRect {
    let origin_x = reference_rect.x as f64 + reference_rect.width as f64 * transform.origin.x;
    let origin_y = reference_rect.y as f64 + reference_rect.height as f64 * transform.origin.y;

    let left = origin_x + (rect.x as f64 - origin_x) * transform.scale_x + transform.translate_x;
    let top = origin_y + (rect.y as f64 - origin_y) * transform.scale_y + transform.translate_y;
    let rect_right = rect.x as f64 + rect.width as f64;
    let rect_bottom = rect.y as f64 + rect.height as f64;
    let right = origin_x + (rect_right - origin_x) * transform.scale_x + transform.translate_x;
    let bottom = origin_y + (rect_bottom - origin_y) * transform.scale_y + transform.translate_y;

    PreciseLogicalRect {
        x: left.min(right) as f32,
        y: top.min(bottom) as f32,
        width: (left.max(right) - left.min(right)).max(0.0) as f32,
        height: (top.max(bottom) - top.min(bottom)).max(0.0) as f32,
    }
}

pub fn inverse_transform_point(
    point: Point<f64, Logical>,
    rect: LogicalRect,
    transform: WindowTransform,
) -> Point<f64, Logical> {
    let origin_x = rect.x as f64 + rect.width as f64 * transform.origin.x;
    let origin_y = rect.y as f64 + rect.height as f64 * transform.origin.y;
    let scale_x = if transform.scale_x.abs() < f64::EPSILON {
        1.0
    } else {
        transform.scale_x
    };
    let scale_y = if transform.scale_y.abs() < f64::EPSILON {
        1.0
    } else {
        transform.scale_y
    };

    Point::from((
        origin_x + (point.x - transform.translate_x - origin_x) / scale_x,
        origin_y + (point.y - transform.translate_y - origin_y) / scale_y,
    ))
}

pub fn transform_point(
    point: Point<f64, Logical>,
    rect: LogicalRect,
    transform: WindowTransform,
) -> Point<f64, Logical> {
    let origin_x = rect.x as f64 + rect.width as f64 * transform.origin.x;
    let origin_y = rect.y as f64 + rect.height as f64 * transform.origin.y;

    Point::from((
        origin_x + (point.x - origin_x) * transform.scale_x + transform.translate_x,
        origin_y + (point.y - origin_y) * transform.scale_y + transform.translate_y,
    ))
}
