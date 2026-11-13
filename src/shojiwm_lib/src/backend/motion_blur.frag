//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

// highp: a mediump step of a fraction of a texel is lost on a wide output.
precision highp float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform vec2 blur_step_uv;
uniform float blur_taps;

varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Taps sit at the midpoints of equal parts of the streak, so their mean is
// the box a shutter leaves, and two taps a hair apart are the pixel itself.
void main() {
    vec2 uv = v_coords - blur_step_uv * (blur_taps - 1.0) * 0.5;
    vec4 sum = vec4(0.0);
    for (int tap = 0; tap < 16; tap++) {
        if (float(tap) >= blur_taps) {
            break;
        }
        sum += texture2D(tex, uv + blur_step_uv * float(tap));
    }
    vec4 color = vec4(sum.rgb / blur_taps, 1.0) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
