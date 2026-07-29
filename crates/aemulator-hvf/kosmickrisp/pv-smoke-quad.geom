#version 450
/* A four-vertex strip: libpoly's is_strip_topology() matches 0,1,2,3, so this is
 * POLY_GS_SHAPE_STATIC_PER_PRIM — a NON-INDEXED draw whose vertices Metal reads
 * in emission order. There is no index buffer to reorder, so last-vertex
 * provoking has to give the draw one (kk_upload_gs_rotated_indices()).
 *
 * The quad is the whole framebuffer, split by its TL-BR diagonal:
 *   triangle 0 = e0,e1,e2 -> the upper-left half
 *   triangle 1 = the lower-right half
 * so a sample near either corner names one triangle unambiguously.
 */
layout(points) in;
layout(triangle_strip, max_vertices = 4) out;

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
   emit(3u, vec2(1.0, 1.0));
   EndPrimitive();
}
