#version 450
/* points in -> triangle_strip out, but three vertices per primitive and two
 * primitives per invocation.
 *
 * That emits 0,1,2,-1,3,4,5 which libpoly's match_list_topology() recognises, so
 * the shape becomes POLY_GS_SHAPE_STATIC_PER_INSTANCE, the declared
 * triangle_strip is rewritten to a triangle LIST, and the restart index is
 * dropped. This is the second shape KosmicKrisp implements, and it is the one
 * that proves the strip -> list rewrite happened: if the driver drew this as a
 * strip instead of a list it would stitch the two triangles together and paint
 * pixels between them. */
layout(points) in;
layout(triangle_strip, max_vertices = 6) out;

layout(location = 0) in vec3 v_tint[];
layout(location = 0) out vec3 g_color;

void main()
{
   float h = 0.24;
   vec4 c = gl_in[0].gl_Position;
   vec3 tint = vec3(v_tint[0].r, 0.5, float(gl_PrimitiveIDIn + 1) / 4.0);

   /* Lower-left triangle. */
   gl_Position = c + vec4(-h, -h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4( 0.0, -h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4(-h,  0.0, 0.0, 0.0); g_color = tint; EmitVertex();
   EndPrimitive();

   /* Upper-right triangle, disjoint from the first. */
   gl_Position = c + vec4( h,  h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4( 0.0,  h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4( h,  0.0, 0.0, 0.0); g_color = tint; EmitVertex();
   EndPrimitive();
}
