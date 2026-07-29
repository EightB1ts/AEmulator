#version 450
/* A geometry shader whose output topology is NOT statically known: the number of
 * emitted vertices depends on a per-primitive value, so
 * nir_gs_count_vertices_and_primitives() cannot fold it and libpoly falls back
 * to POLY_GS_SHAPE_DYNAMIC_INDEXED — the MAIN geometry program writes an index
 * buffer at draw time, as strips separated by a restart index.
 *
 * Metal has no primitive restart, so KosmicKrisp unrolls that index buffer into
 * independent triangles (libkk_unroll_geometry) before drawing it. This shader
 * used to exist only to be REFUSED; it is now a positive test, and what it
 * proves is that the unroll preserved BOTH the geometry and the encoded vertex
 * IDs that the rasterization shader inverts back into (input primitive, output
 * vertex).
 *
 * Every input point grows a quad. Odd-numbered input points grow a second quad
 * below the first, sharing its bottom edge — i.e. the strip continues, and its
 * length is data dependent. An implementation that quietly emitted a fixed 6
 * vertices would paint the extension under the even points too, and one that
 * emitted a fixed 4 would leave it off the odd ones. */
layout(points) in;
layout(triangle_strip, max_vertices = 6) out;

layout(location = 0) in vec3 v_tint[];
layout(location = 0) out vec3 g_color;

void main()
{
   float h = 0.24;
   vec4 c = gl_in[0].gl_Position;

   /* Blue channel encodes the input primitive index so a mis-inverted vertex ID
    * shows up as a wrong colour rather than as correct-looking geometry. */
   vec3 tint = vec3(v_tint[0].r, 0.5, float(gl_PrimitiveIDIn + 1) / 4.0);

   /* Loop bound is not a compile-time constant: 4 vertices (one quad) for even
    * input primitives, 6 (two stacked quads) for odd ones. */
   int n = 4 + 2 * (gl_PrimitiveIDIn & 1);

   for (int i = 0; i < n; ++i) {
      /* Rows of two, walking down in NDC +y: (-h,-h) (h,-h) (-h,h) (h,h)
       * (-h,3h) (h,3h). As a triangle strip that is the quad, then the quad
       * below it. */
      float x = ((i & 1) == 0) ? -h : h;
      float y = -h + float(i / 2) * 2.0 * h;

      gl_Position = c + vec4(x, y, 0.0, 0.0);
      g_color = tint;
      EmitVertex();
   }
   EndPrimitive();
}
