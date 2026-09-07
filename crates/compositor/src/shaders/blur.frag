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
    vec4 c = texture2D(tex, v_coords) * 0.25;
    c += texture2D(tex, v_coords + vec2(wm_step.x, 0.0)) * 0.125;
    c += texture2D(tex, v_coords - vec2(wm_step.x, 0.0)) * 0.125;
    c += texture2D(tex, v_coords + vec2(0.0, wm_step.y)) * 0.125;
    c += texture2D(tex, v_coords - vec2(0.0, wm_step.y)) * 0.125;
    c += texture2D(tex, v_coords + wm_step) * 0.0625;
    c += texture2D(tex, v_coords - wm_step) * 0.0625;
    c += texture2D(tex, v_coords + vec2(wm_step.x, -wm_step.y)) * 0.0625;
    c += texture2D(tex, v_coords + vec2(-wm_step.x, wm_step.y)) * 0.0625;
    vec2 half_size = wm_rect.zw * 0.5;
    float radius = min(wm_radius, min(half_size.x, half_size.y));
    vec2 p = abs(gl_FragCoord.xy - wm_rect.xy - half_size) - half_size + radius;
    float d = length(max(p, 0.0)) + min(max(p.x, p.y), 0.0) - radius;
    gl_FragColor = c * alpha * (1.0 - smoothstep(-0.75, 0.75, d));
}
