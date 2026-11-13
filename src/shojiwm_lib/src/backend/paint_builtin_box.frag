// The built-in background and border, written against the same contract as
// user paint shaders.
vec4 paint_main(PaintContext ctx) {
    vec4 color = vec4(0.0);
    if (shoji_box_fill > 0.5) {
        color = ctx.background * shoji_fill_coverage(ctx);
    }
    if (shoji_box_border > 0.5) {
        float fade = mix(1.0, shoji_border_fade, clamp(ctx.frag_phy_px.y / max(ctx.size_phy_px.y, 1.0), 0.0, 1.0));
        color = shoji_over(shoji_border_color_at(ctx) * (shoji_border_coverage(ctx) * fade), color);
    }
    return color;
}
