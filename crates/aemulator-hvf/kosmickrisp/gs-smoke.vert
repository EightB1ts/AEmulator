#version 450
/* One point per invocation, laid out left to right across the top half of NDC.
 * Nothing here is interesting; the point of the test is what the geometry shader
 * does with these, and that the vertex stage survives being run as a compute
 * prepass writing to a buffer instead of feeding the rasterizer directly. */
layout(location = 0) out vec3 v_tint;

void main()
{
   float n = 4.0;
   float x = (float(gl_VertexIndex) + 0.5) / n * 2.0 - 1.0;
   gl_Position = vec4(x, 0.0, 0.0, 1.0);

   /* A per-input-vertex value the geometry shader must forward, so a GS that
    * drops its inputs cannot pass by accident. */
   v_tint = vec3(float(gl_VertexIndex + 1) / n, 0.0, 0.0);
}
