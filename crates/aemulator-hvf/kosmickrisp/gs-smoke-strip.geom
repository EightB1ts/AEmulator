#version 450
/* points in -> triangle_strip out, four vertices, one EndPrimitive.
 *
 * The output index pattern is 0,1,2,3 which libpoly's is_strip_topology()
 * recognises, so poly_nir_lower_gs() picks POLY_GS_SHAPE_STATIC_PER_PRIM: no
 * index buffer, no primitive restart, hardware instancing over input primitives.
 * That is the shape KosmicKrisp implements today, and this is the billboard
 * expansion that most real geometry shaders are.
 *
 * gl_PrimitiveIDIn is written to a varying so the test can prove that the RAST
 * vertex shader recovered the right input primitive from its instance ID. */
layout(points) in;
layout(triangle_strip, max_vertices = 4) out;

layout(location = 0) in vec3 v_tint[];
layout(location = 0) out vec3 g_color;

void main()
{
   float h = 0.24;
   vec4 c = gl_in[0].gl_Position;

   /* Blue channel encodes the input primitive index so a mis-inverted instance
    * ID shows up as a wrong colour rather than as correct-looking geometry. */
   vec3 tint = vec3(v_tint[0].r, 0.5, float(gl_PrimitiveIDIn + 1) / 4.0);

   gl_Position = c + vec4(-h, -h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4(-h,  h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4( h, -h, 0.0, 0.0); g_color = tint; EmitVertex();
   gl_Position = c + vec4( h,  h, 0.0, 0.0); g_color = tint; EmitVertex();
   EndPrimitive();
}
