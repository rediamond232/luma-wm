precision highp float;
uniform float alpha;
uniform vec4 wm_rect;
uniform vec4 wm_color;
uniform float wm_radius;
uniform float wm_width;
uniform float wm_shadow_size;
uniform float wm_shadow_opacity;
float rounded(vec2 point, vec2 half_size, float radius) {
    radius = min(radius, min(half_size.x, half_size.y));
    vec2 p = abs(point) - half_size + radius;
    return length(max(p, 0.0)) + min(max(p.x, p.y), 0.0) - radius;
}
void main() {
    vec2 half_size = wm_rect.zw * 0.5;
    vec2 p = gl_FragCoord.xy - wm_rect.xy - half_size;
    float distance = rounded(p, half_size, wm_radius);
    float outer = 1.0 - smoothstep(-0.75, 0.75, distance);
    float inner = smoothstep(-0.75, 0.75, rounded(p, max(half_size - wm_width, 0.0), max(wm_radius - wm_width, 0.0)));
    float a = wm_width > 0.0 ? wm_color.a * alpha * outer * inner : 0.0;
    // Analytic exterior falloff: no texture capture or additional blur pass.
    float shadow = wm_shadow_size > 0.0 ? wm_shadow_opacity * alpha
        * (1.0 - smoothstep(0.0, max(wm_shadow_size, 0.001), distance))
        * smoothstep(-0.75, 0.75, distance) : 0.0;
    gl_FragColor = vec4(wm_color.rgb * a, a + shadow * (1.0 - a));
}
