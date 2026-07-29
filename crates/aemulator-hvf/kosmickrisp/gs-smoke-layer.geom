#version 450
/* Same static strip expansion as gs-smoke-strip.geom, but each input primitive
 * picks a render target layer.
 *
 * This is the semantic the compute-prepass model is most likely to get wrong.
 * gl_Layer is a per-VERTEX geometry shader output in GLSL/SPIR-V; Vulkan and GL
 * then take the value from the primitive's PROVOKING vertex. In this model the
 * RAST vertex shader emits it per output vertex and Metal reads it from a
 * [[render_target_array_index]] vertex output — and Metal requires that value to
 * be identical for every vertex of a primitive, leaving it undefined otherwise.
 * So a geometry shader that sets gl_Layer once per primitive (as here, and as
 * essentially all real ones do) is exact, and one that varies gl_Layer between
 * the vertices of a single primitive is undefined on Metal where Vulkan would
 * have defined it as the provoking vertex's value. That gap is not testable from
 * here; what is testable is that the common case routes correctly. */
layout(points) in;
layout(triangle_strip, max_vertices = 4) out;

layout(location = 0) in vec3 v_tint[];
layout(location = 0) out vec3 g_color;

void main()
{
   float h = 0.24;
   vec4 c = gl_in[0].gl_Position;
   vec3 tint = vec3(v_tint[0].r, 0.5, float(gl_PrimitiveIDIn + 1) / 4.0);
   int layer = gl_PrimitiveIDIn & 1;

   gl_Position = c + vec4(-h, -h, 0.0, 0.0); g_color = tint; gl_Layer = layer; EmitVertex();
   gl_Position = c + vec4(-h,  h, 0.0, 0.0); g_color = tint; gl_Layer = layer; EmitVertex();
   gl_Position = c + vec4( h, -h, 0.0, 0.0); g_color = tint; gl_Layer = layer; EmitVertex();
   gl_Position = c + vec4( h,  h, 0.0, 0.0); g_color = tint; gl_Layer = layer; EmitVertex();
   EndPrimitive();
}
