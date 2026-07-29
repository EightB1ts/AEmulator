#version 450
/* One point at the centre. Everything the geometry-shader cases draw is emitted
 * by the geometry shader, so this stage exists only to give it an input
 * primitive — and, because a geometry shader turns the vertex stage into a
 * compute prepass, to make sure that prepass runs at all.
 */
void main()
{
   gl_Position = vec4(0.0, 0.0, 0.0, 1.0);
   gl_PointSize = 1.0;
}
