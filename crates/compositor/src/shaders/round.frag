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
uniform vec4 wm_rect;
uniform float wm_radius;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color.a = 1.0;
#endif
    vec2 half_size = wm_rect.zw * 0.5;
    float radius = min(wm_radius, min(half_size.x, half_size.y));
    vec2 p = abs(gl_FragCoord.xy - wm_rect.xy - half_size) - half_size + radius;
    float distance = length(max(p, 0.0)) + min(max(p.x, p.y), 0.0) - radius;
    gl_FragColor = color * alpha * (1.0 - smoothstep(-0.75, 0.75, distance));
}
