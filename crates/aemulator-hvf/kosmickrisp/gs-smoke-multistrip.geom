#version 450
/* Two separate four-vertex strips, both statically known.
 *
 * The emitted index pattern is 0,1,2,3,-1,4,5,6,7: an integer number of
 * primitives per strip, so match_list_topology() rejects it, and not the
 * identity, so is_strip_topology() rejects it too. libpoly therefore picks
 * POLY_GS_SHAPE_STATIC_INDEXED and hands the driver that constant index buffer
 * — restart index and all.
 *
 * Metal has no primitive restart, so KosmicKrisp unrolls the constant at
 * pipeline-compile time (kk_gs_unroll_static_topology) into the equivalent
 * triangle list. This was refused before that landed.
 *
 * The two quads are stacked with a gap between them, and the test checks the gap
 * is background: if the restart were dropped instead of honoured, the strip
 * would run 0..7 continuously and stitch the two quads together straight through
 * that gap. */
layout(points) in;
layout(triangle_strip, max_vertices = 8) out;

layout(location = 0) in vec3 v_tint[];
layout(location = 0) out vec3 g_color;

void main()
{
   float h = 0.14;
   vec4 c = gl_in[0].gl_Position;
   vec3 tint = vec3(v_tint[0].r, 0.5, float(gl_PrimitiveIDIn + 1) / 4.0);

   /* Upper quad: y in [-3h, -h]. */
   gl_Position = c + vec4(-h, -3.0 * h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4( h, -3.0 * h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4(-h, -h, 0.0, 0.0);       g_color = tint; EmitVertex();
   gl_Position = c + vec4( h, -h, 0.0, 0.0);       g_color = tint; EmitVertex();
   EndPrimitive();

   /* Lower quad: y in [h, 3h]. The band between -h and h is the discriminator. */
   gl_Position = c + vec4(-h, h, 0.0, 0.0);        g_color = tint; EmitVertex();
   gl_Position = c + vec4( h, h, 0.0, 0.0);        g_color = tint; EmitVertex();
   gl_Position = c + vec4(-h, 3.0 * h, 0.0, 0.0);  g_color = tint; EmitVertex();
   gl_Position = c + vec4( h, 3.0 * h, 0.0, 0.0);  g_color = tint; EmitVertex();
   EndPrimitive();
}
