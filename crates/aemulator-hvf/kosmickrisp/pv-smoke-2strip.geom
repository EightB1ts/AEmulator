#version 450
/* Two disjoint four-vertex strips. Neither pattern matches, so libpoly falls
 * back to POLY_GS_SHAPE_STATIC_INDEXED: a constant index buffer with a RESTART
 * index between the strips, which KosmicKrisp unrolls on the CPU
 * (kk_gs_unroll_static_topology()). That is the site whose own comment named
 * this hazard.
 *
 * Left quad = the left half of the framebuffer, right quad = the right half,
 * each split by its own diagonal, so all four output triangles are sampleable.
 */
layout(points) in;
layout(triangle_strip, max_vertices = 8) out;

layout(location = 0) flat out uint v_vid;
layout(location = 1) out float v_shade;

void emit(uint id, vec2 p)
{
   gl_Position = vec4(p, 0.0, 1.0);
   v_vid = id;
   v_shade = float(id) / 8.0;
   EmitVertex();
}

void main()
{
   emit(0u, vec2(-1.0, -1.0));
   emit(1u, vec2(-1.0, 1.0));
   emit(2u, vec2(0.0, -1.0));
   emit(3u, vec2(0.0, 1.0));
   EndPrimitive();

   emit(4u, vec2(0.0, -1.0));
   emit(5u, vec2(0.0, 1.0));
   emit(6u, vec2(1.0, -1.0));
   emit(7u, vec2(1.0, 1.0));
   EndPrimitive();
}
