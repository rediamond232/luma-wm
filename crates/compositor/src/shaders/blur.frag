#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
uniform vec2 wm_step;
uniform vec4 wm_rect;
uniform float wm_radius;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
void main() {
    // Concentric, rotated sample rings avoid turning repeating wallpaper
    // details into the plaid pattern produced by a regular sparse grid.
    vec4 c = texture2D(tex, v_coords) * 0.12;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.5000,  0.0000)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.3536,  0.3536)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.0000,  0.5000)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2(-0.3536,  0.3536)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2(-0.5000,  0.0000)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2(-0.3536, -0.3536)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.0000, -0.5000)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.3536, -0.3536)) * 0.06;
    c += texture2D(tex, v_coords + wm_step * vec2( 1.1549,  0.4784)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.4784,  1.1549)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2(-0.4784,  1.1549)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2(-1.1549,  0.4784)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2(-1.1549, -0.4784)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2(-0.4784, -1.1549)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.4784, -1.1549)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2( 1.1549, -0.4784)) * 0.035;
    c += texture2D(tex, v_coords + wm_step * vec2( 2.2068,  0.4389)) * 0.015;
    c += texture2D(tex, v_coords + wm_step * vec2( 1.2500,  1.8709)) * 0.015;
    c += texture2D(tex, v_coords + wm_step * vec2(-0.4389,  2.2068)) * 0.015;
    c += texture2D(tex, v_coords + wm_step * vec2(-1.8709,  1.2500)) * 0.015;
    c += texture2D(tex, v_coords + wm_step * vec2(-2.2068, -0.4389)) * 0.015;
    c += texture2D(tex, v_coords + wm_step * vec2(-1.2500, -1.8709)) * 0.015;
    c += texture2D(tex, v_coords + wm_step * vec2( 0.4389, -2.2068)) * 0.015;
    c += texture2D(tex, v_coords + wm_step * vec2( 1.8709, -1.2500)) * 0.015;
    vec2 half_size = wm_rect.zw * 0.5;
    float radius = min(wm_radius, min(half_size.x, half_size.y));
    vec2 p = abs(gl_FragCoord.xy - wm_rect.xy - half_size) - half_size + radius;
    float d = length(max(p, 0.0)) + min(max(p.x, p.y), 0.0) - radius;
    gl_FragColor = c * alpha * (1.0 - smoothstep(-0.75, 0.75, d));
}
