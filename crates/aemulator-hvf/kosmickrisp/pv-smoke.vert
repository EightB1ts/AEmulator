#version 450
/* Vertex positions from gl_VertexIndex alone — there is no vertex buffer, so a
 * draw is fully described by its topology and its vertex count and the test can
 * reuse one pipeline layout everywhere.
 *
 * The point of the whole test is WHICH vertex of a primitive the rasterizer
 * calls provoking, so the only interesting output is v_vid: the vertex's own
 * index, flat-interpolated. Whatever value comes out the other side of the
 * fragment shader names the vertex that won.
 *
 * v_shade is the same information smoothly interpolated, and exists so that
 * pv-smoke-smooth.frag can be linked against this same vertex shader. A
 * fragment shader that reads only v_shade has no flat input at all, which is
 * exactly the case KosmicKrisp is allowed to skip the provoking-vertex work
 * for — see kk_provoking_observable().
 */
layout(push_constant) uniform PC {
   /* 0: a 3x2 ribbon, the layout for strips and lists. 1: three points across
    * the middle row, for line topologies. 2: a fan around the centre. */
   uint layout_id;
} pc;

layout(location = 0) flat out uint v_vid;
layout(location = 1) out float v_shade;

void main()
{
   uint i = uint(gl_VertexIndex);
   v_vid = i;
   v_shade = float(i) / 8.0;

   vec2 p;
   if (pc.layout_id == 1u) {
      /* (-1,0) (0,0) (1,0): two horizontal segments across the middle. */
      p = vec2(-1.0 + float(i), 0.0);
   } else if (pc.layout_id == 2u) {
      /* Centre, then three corners walking clockwise in framebuffer space, so
       * that the two fan triangles are disjoint wedges. */
      p = (i == 0u) ? vec2(0.0, 0.0)
                    : vec2((i == 1u) ? -1.0 : 1.0, (i <= 2u) ? -1.0 : 1.0);
   } else {
      /* Column k = i/2 at x = -1 + k, top (y=-1) for even i and bottom (y=+1)
       * for odd i. Six vertices give a strip of four triangles that tile two
       * squares along the diagonal, each triangle sampleable on its own. */
      p = vec2(-1.0 + float(i / 2u), ((i % 2u) == 0u) ? -1.0 : 1.0);
   }

   gl_Position = vec4(p, 0.0, 1.0);
}
