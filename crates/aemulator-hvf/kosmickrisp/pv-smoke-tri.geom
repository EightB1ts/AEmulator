#version 450
/* A three-vertex strip, which libpoly's match_list_topology() rewrites into an
 * independent TRIANGLE LIST: POLY_GS_SHAPE_STATIC_PER_INSTANCE. Also
 * non-indexed, and unlike the strip case its vertex count scales with the number
 * of input primitives — which is the one place the last-vertex emulation costs
 * something proportional to the draw.
 *
 * One triangle, the upper-left half of the framebuffer.
 */
layout(points) in;
layout(triangle_strip, max_vertices = 3) out;

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
   emit(2u, vec2(1.0, -1.0));
   EndPrimitive();
}
