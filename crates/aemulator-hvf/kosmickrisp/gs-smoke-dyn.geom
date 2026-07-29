#version 450
/* A geometry shader whose output topology is NOT statically known: the number of
 * emitted vertices depends on a per-primitive value, so libpoly falls back to
 * POLY_GS_SHAPE_DYNAMIC_INDEXED and the generated index buffer contains restart
 * indices, which Metal has no way to consume.
 *
 * This shader exists to be REFUSED. The test asserts that creating the pipeline
 * returns an error instead of rendering something plausible-but-wrong, because a
 * driver that silently drops a geometry shader is much worse than one that says
 * no. When the unroll path lands, this file becomes a positive test. */
layout(points) in;
layout(triangle_strip, max_vertices = 8) out;

layout(location = 0) in vec3 v_tint[];
layout(location = 0) out vec3 g_color;

void main()
{
   float h = 0.24;
   vec4 c = gl_in[0].gl_Position;

   /* Loop bound is data dependent, so nir_gs_count_vertices_and_primitives
    * cannot fold it. */
   int n = 3 + int(v_tint[0].r * 4.0);
   for (int i = 0; i < n; ++i) {
      gl_Position = c + vec4(-h + float(i) * 0.1, -h, 0.0, 0.0);
      g_color = vec3(1.0, 0.0, 0.0);
      EmitVertex();
   }
   EndPrimitive();
}
