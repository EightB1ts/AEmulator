#version 450
/* The same geometry with NO flat input anywhere in the pipeline.
 *
 * The provoking vertex cannot affect a smoothly interpolated value, so this
 * render must come out identical in both modes — which is what makes it legal
 * for the driver to skip the primitive decomposition here. The test compares the
 * two images byte for byte: if last-vertex mode ever changed rasterization
 * (winding, tie-breaking, coverage) rather than only which vertex is provoking,
 * this is where it shows up.
 */
layout(location = 1) in float v_shade;
layout(location = 0) out vec4 o_color;

void main()
{
   o_color = vec4(v_shade, 1.0, 0.0, 1.0);
}
