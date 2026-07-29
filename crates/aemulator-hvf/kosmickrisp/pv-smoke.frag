#version 450
/* The readback contract for the whole test.
 *
 *   RED   = the provoking vertex's index + 1, so 0 can mean "background".
 *   GREEN = 255 everywhere a fragment ran, so coverage can be compared between
 *           two renders independently of which vertex won.
 *
 * v_vid is flat, which is the only reason any of this depends on the provoking
 * vertex — and the only reason KosmicKrisp does the decomposition at all.
 */
layout(location = 0) flat in uint v_vid;
layout(location = 0) out vec4 o_color;

void main()
{
   o_color = vec4(float(v_vid + 1u) / 255.0, 1.0, 0.0, 1.0);
}
