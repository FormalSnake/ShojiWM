uniform vec2 blur_step_uv;
uniform float blur_taps;

// Taps sit at the midpoints of equal parts of the streak, so their mean is
// the box a shutter leaves, and two taps a hair apart are the pixel itself.
vec4 shader_main(EffectContext effect) {
    vec2 uv = effect.texture_uv - blur_step_uv * (blur_taps - 1.0) * 0.5;
    vec4 sum = vec4(0.0);
    for (int tap = 0; tap < 16; tap++) {
        if (float(tap) >= blur_taps) {
            break;
        }
        sum += texture2D(tex, uv + blur_step_uv * float(tap));
    }
    return sum / blur_taps;
}
