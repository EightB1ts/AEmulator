#version 450
/* The same full-framebuffer quad as pv-smoke-quad.geom, but emitted from a loop
 * whose bound is not a compile-time constant, so nir_gs_count_vertices_and_
 * primitives() cannot fold it and libpoly falls back to
 * POLY_GS_SHAPE_DYNAMIC_INDEXED: the MAIN geometry program writes an index
 * buffer of restart-separated strips at draw time, and libkk_unroll_geometry
 * rewrites it into independent primitives on the GPU.
 *
 * That is the one producer of decomposed primitives that runs entirely on the
 * GPU, so it is the one whose provoking-vertex order has to be got right in the
 * kernel's arguments rather than in C. With a single input point the geometry is
 * identical to the static strip case, which is the point: same picture, same
 * expected provoking vertices, different code path.
 */
layout(points) in;
layout(triangle_strip, max_vertices = 6) out;

layout(location = 0) flat out uint v_vid;
layout(location = 1) out float v_shade;

void main()
{
   /* 4 for input primitive 0, 6 for primitive 1 — the test draws one point, so
    * the shape is the quad, but the count is not statically knowable. */
   int n = 4 + 2 * (gl_PrimitiveIDIn & 1);

   for (int i = 0; i < n; ++i) {
      /* The same four corners, in the same order, as pv-smoke-quad.geom. */
      float x = (i < 2) ? -1.0 : 1.0;
      float y = ((i & 1) == 0) ? -1.0 : 1.0;

      gl_Position = vec4(x, y, 0.0, 1.0);
      v_vid = uint(i);
      v_shade = float(i) / 8.0;
      EmitVertex();
   }
   EndPrimitive();
}
