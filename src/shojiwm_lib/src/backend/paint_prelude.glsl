precision highp float;

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

// Geometry, in whole physical pixels. Rects are (x, y, width, height); the
// node rect is relative to the drawn area, every other rect to the node.
uniform vec4 shoji_node_rect;
uniform vec4 shoji_border;        // top, right, bottom, left
uniform vec4 shoji_radius;        // top-left, top-right, bottom-right, bottom-left
uniform vec4 shoji_padding;       // top, right, bottom, left
uniform vec4 shoji_content_rect;
uniform vec4 shoji_inner_rect;
uniform vec4 shoji_inner_radius;
uniform float shoji_hole;
uniform float shoji_scale;

// Colors, premultiplied.
uniform vec4 shoji_background;
uniform vec4 shoji_border_color_top;
uniform vec4 shoji_border_color_right;
uniform vec4 shoji_border_color_bottom;
uniform vec4 shoji_border_color_left;
// The border's alpha at the bottom edge as a share of its top's.
uniform float shoji_border_fade;
uniform float shoji_opacity;

// Clips applied after `paint_main`, relative to the node.
uniform float shoji_clip_enabled;
uniform vec4 shoji_clip_rect;
uniform vec4 shoji_clip_radius;
uniform float shoji_rounded_clip_enabled;
uniform vec4 shoji_rounded_clip_rect;
uniform vec4 shoji_rounded_clip_radius;

// Parameters of the built-in programs.
uniform float shoji_box_fill;
uniform float shoji_box_border;
uniform vec4 shoji_shadow_color;
uniform vec2 shoji_shadow_offset;
uniform float shoji_shadow_sigma;
uniform float shoji_shadow_spread;
uniform float shoji_shadow_inset;

struct PaintContext {
    vec2 frag_phy_px;          // this fragment, from the node's top-left (pixel centers at .5)
    vec2 size_phy_px;          // the node's border box
    vec4 border_phy_px;        // top, right, bottom, left
    vec4 radius_phy_px;        // top-left, top-right, bottom-right, bottom-left
    vec4 padding_phy_px;       // top, right, bottom, left
    vec4 content_rect_phy_px;  // inside border and padding: x, y, width, height
    vec4 inner_rect_phy_px;    // the inner edge of the border
    vec4 inner_radius_phy_px;
    bool has_hole;             // the inner area shows the window's client surface
    vec4 background;           // premultiplied
    vec4 border_color;         // premultiplied (the top side)
    float scale;               // physical pixels per logical pixel
};

// Signed distance to a rounded rect (negative inside). `rect` is
// (x, y, width, height); `radius` is top-left, top-right, bottom-right,
// bottom-left.
float shoji_rrect_sdf(vec2 p, vec4 rect, vec4 radius) {
    vec2 half_size = rect.zw * 0.5;
    vec2 q = p - rect.xy - half_size;
    float r;
    if (q.x >= 0.0) {
        r = q.y >= 0.0 ? radius.z : radius.y;
    } else {
        r = q.y >= 0.0 ? radius.w : radius.x;
    }
    r = min(r, min(half_size.x, half_size.y));
    vec2 d = abs(q) - (half_size - vec2(r));
    return min(max(d.x, d.y), 0.0) + length(max(d, 0.0)) - r;
}

// Pixel coverage of a signed distance, antialiased over one physical pixel.
// Straight edges on the pixel grid come out fully crisp.
float shoji_coverage(float sdf) {
    return clamp(0.5 - sdf, 0.0, 1.0);
}

float shoji_rrect_coverage(vec2 p, vec4 rect, vec4 radius) {
    if (rect.z <= 0.0 || rect.w <= 0.0) {
        return 0.0;
    }
    return shoji_coverage(shoji_rrect_sdf(p, rect, radius));
}

// Coverage of the node's border box.
float shoji_outer_coverage(PaintContext ctx) {
    return shoji_rrect_coverage(ctx.frag_phy_px, vec4(vec2(0.0), ctx.size_phy_px), ctx.radius_phy_px);
}

// Coverage of the area inside the border.
float shoji_inner_coverage(PaintContext ctx) {
    return shoji_rrect_coverage(ctx.frag_phy_px, ctx.inner_rect_phy_px, ctx.inner_radius_phy_px);
}

float shoji_border_coverage(PaintContext ctx) {
    return max(shoji_outer_coverage(ctx) - shoji_inner_coverage(ctx), 0.0);
}

// Where the background goes: the border box, minus the client surface area
// of a <WindowBorder>.
float shoji_fill_coverage(PaintContext ctx) {
    float outer = shoji_outer_coverage(ctx);
    if (ctx.has_hole) {
        return max(outer - shoji_inner_coverage(ctx), 0.0);
    }
    return outer;
}

// The border color of the side the fragment belongs to (corners split along
// the diagonal between the two sides).
vec4 shoji_border_color_at(PaintContext ctx) {
    vec2 p = ctx.frag_phy_px;
    vec4 b = max(ctx.border_phy_px, vec4(0.0001));
    float top = p.y / b.x;
    float right = (ctx.size_phy_px.x - p.x) / b.y;
    float bottom = (ctx.size_phy_px.y - p.y) / b.z;
    float left = p.x / b.w;
    float nearest = min(min(top, right), min(bottom, left));
    if (nearest == top && ctx.border_phy_px.x > 0.0) {
        return shoji_border_color_top;
    }
    if (nearest == right && ctx.border_phy_px.y > 0.0) {
        return shoji_border_color_right;
    }
    if (nearest == bottom && ctx.border_phy_px.z > 0.0) {
        return shoji_border_color_bottom;
    }
    if (nearest == left && ctx.border_phy_px.w > 0.0) {
        return shoji_border_color_left;
    }
    return ctx.border_color;
}

vec2 shoji_erf(vec2 x) {
    vec2 s = sign(x);
    vec2 a = abs(x);
    x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    x *= x;
    return s - s / (x * x);
}

float shoji_gaussian(float x, float sigma) {
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (2.5066282746 * sigma);
}

float shoji_blurred_rrect_row(float x, float y, float sigma, float corner, vec2 half_size) {
    float delta = min(half_size.y - corner - abs(y), 0.0);
    float curved = half_size.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    vec2 integral =
        0.5 + 0.5 * shoji_erf((x + vec2(-curved, curved)) * (0.7071067811865476 / sigma));
    return integral.y - integral.x;
}

// Coverage of a rounded rect blurred by a Gaussian of `sigma` pixels
// (analytic along x, four samples along y). Sharp when sigma is ~0.
float shoji_blurred_rrect_coverage(vec2 p, vec4 rect, vec4 radius, float sigma) {
    if (rect.z <= 0.0 || rect.w <= 0.0) {
        return 0.0;
    }
    if (sigma < 0.05) {
        return shoji_rrect_coverage(p, rect, radius);
    }
    vec2 half_size = rect.zw * 0.5;
    vec2 q = p - rect.xy - half_size;
    float corner;
    if (q.x >= 0.0) {
        corner = q.y >= 0.0 ? radius.z : radius.y;
    } else {
        corner = q.y >= 0.0 ? radius.w : radius.x;
    }
    corner = min(corner, min(half_size.x, half_size.y));
    float low = q.y - half_size.y;
    float high = q.y + half_size.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);
    float step = (end - start) / 4.0;
    float y = start + step * 0.5;
    float value = 0.0;
    for (int i = 0; i < 4; i++) {
        value += shoji_blurred_rrect_row(q.x, q.y - y, sigma, corner, half_size)
            * shoji_gaussian(y, sigma) * step;
        y += step;
    }
    return clamp(value, 0.0, 1.0);
}

// `src` over `dst`, both premultiplied.
vec4 shoji_over(vec4 src, vec4 dst) {
    return src + dst * (1.0 - src.a);
}

vec4 shoji_premultiply(vec4 color) {
    return vec4(color.rgb * color.a, color.a);
}

// The built-in look: background, then border.
vec4 shoji_default_paint(PaintContext ctx) {
    vec4 fill = ctx.background * shoji_fill_coverage(ctx);
    vec4 border = shoji_border_color_at(ctx) * shoji_border_coverage(ctx);
    return shoji_over(border, fill);
}

PaintContext shoji_make_paint_context() {
    vec2 draw_px = v_coords * size;
    return PaintContext(
        draw_px - shoji_node_rect.xy,
        shoji_node_rect.zw,
        shoji_border,
        shoji_radius,
        shoji_padding,
        shoji_content_rect,
        shoji_inner_rect,
        shoji_inner_radius,
        shoji_hole > 0.5,
        shoji_background,
        shoji_border_color_top,
        shoji_scale
    );
}

float shoji_clip_coverage(vec2 p) {
    float coverage = 1.0;
    if (shoji_clip_enabled > 0.5) {
        coverage *= shoji_rrect_coverage(p, shoji_clip_rect, shoji_clip_radius);
    }
    if (shoji_rounded_clip_enabled > 0.5) {
        coverage *= shoji_rrect_coverage(p, shoji_rounded_clip_rect, shoji_rounded_clip_radius);
    }
    return coverage;
}
