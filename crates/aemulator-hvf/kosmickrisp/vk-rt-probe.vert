#version 460

/* The vertex half of vk-rt-probe's --fragment mode. It exists only to get one
 * fragment per ray onto an NRAYS x 1 render target, so it takes no vertex
 * buffer and no descriptor: three vertices, drawn as a triangle that covers the
 * whole viewport.
 *
 * Keeping the vertex stage this dumb is the point. --fragment is asking one
 * question — does a ray query work in a FRAGMENT shader, where a real
 * application puts its shadow rays — and anything else this shader did would be
 * another way for the mode to fail for a reason that is not that. */
void main()
{
   /* The standard three-vertex full-viewport triangle: (-1,-1) (3,-1) (-1,3).
    * Larger than the clip volume on purpose, so the clipped result covers every
    * pixel with no seam down the diagonal that a two-triangle quad would have. */
   vec2 p = vec2(float((gl_VertexIndex & 1) << 2) - 1.0,
                 float((gl_VertexIndex & 2) << 1) - 1.0);
   gl_Position = vec4(p, 0.0, 1.0);
}
