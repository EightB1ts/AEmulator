#version 450
/* The control. No geometry shader at all: one triangle covering the middle of
 * the target, straight through the ordinary hardware vertex path.
 *
 * This exists because the geometry-shader work touches shared draw code —
 * build_draw_data(), the per-draw upload mask, the layout of
 * struct kk_per_draw_data, kk_shader_destroy() — and a regression there breaks
 * every ordinary draw in the driver while every geometry-shader check still
 * passes. If this case fails, nothing else in this harness matters. */
layout(location = 0) out vec3 g_color;

void main()
{
   vec2 p[3] = vec2[3](vec2(-0.5, -0.5), vec2(0.5, -0.5), vec2(0.0, 0.5));
   gl_Position = vec4(p[gl_VertexIndex], 0.0, 1.0);
   g_color = vec3(0.0, 1.0, 0.0);
}
