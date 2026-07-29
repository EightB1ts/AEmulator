#version 460
#extension GL_EXT_ray_query : require

/* The fragment half of vk-rt-probe's --fragment mode.
 *
 * WHY A SECOND SHADER EXISTS AT ALL
 * ---------------------------------
 * vk-rt-probe.comp asks the ray-query question from a COMPUTE shader. Every real
 * ray-query application asks it from a FRAGMENT shader, because the whole point
 * of ray query (as opposed to a ray-tracing pipeline) is to trace a secondary
 * ray from inside an ordinary raster shader — a shadow ray from the surface
 * being shaded. The Vulkan sample this probe is calibrated against
 * (SaschaWillems' "Ray queries for ray traced shadows") declares its
 * accelerationStructureEXT in scene.frag with execution model Fragment, and
 * nothing else in the sample traces anything.
 *
 * Those are different code paths in a Vulkan-on-Metal driver: a different
 * pipeline type, a different Metal encoder (render, not compute), a different
 * argument-buffer layout, and a different residency story for the acceleration
 * structure. A compute-only probe can therefore report PASS while every shadow
 * ray in a real application misses. That happened, which is why this file
 * exists.
 *
 * The ray-query BODY is character-for-character the body of vk-rt-probe.comp.
 * That is the invariant worth protecting: with the same triangle, the same eight
 * rays and the same accessors, a compute PASS next to a fragment FAIL localises
 * the bug to the stage and to nothing else. The only differences are where the
 * ray index comes from (gl_FragCoord instead of gl_GlobalInvocationID) and the
 * unused colour output a fragment stage has to have.
 */

/* The ray flags come in as a push constant so the probe can sweep them without
 * a rebuild. They are what an application actually varies here, and they are not
 * cosmetic: KosmicKrisp turns each VkRayFlagBit into a call on Metal's
 * raytracing::intersection_params, so a mis-mapped bit changes the traversal
 * result rather than being ignored. The default the probe passes is
 * gl_RayFlagsOpaqueEXT (0x1); the shadow-ray idiom every real ray-query
 * application uses is gl_RayFlagsTerminateOnFirstHitEXT (0x4). */
layout(push_constant) uniform Params {
   uint rayFlags;
} params;

layout(set = 0, binding = 0) uniform accelerationStructureEXT tlas;

struct Result {
   uint  committed_type;   /* 0 none, 1 triangle, 2 generated */
   float t;
   uint  prim;
   uint  geom;
   uint  instance_id;
   uint  custom_index;
   float bary_x;
   float bary_y;
   uint  front_face;
   uint  candidates_seen;
   float ray_tmin;
   uint  ray_flags;
};

layout(set = 0, binding = 1, std430) buffer Out {
   Result r[];
} outbuf;

/* Written but never read. A fragment shader whose only side effect is a storage
 * write is legal, but an unwritten attachment invites a driver to decide the
 * whole draw is dead, and "the optimiser deleted the ray query" is not a result
 * this probe should be able to mistake for a miss. */
layout(location = 0) out vec4 outColor;

void main()
{
   /* One fragment per ray: the render target is NRAYS x 1, so gl_FragCoord.x is
    * 0.5, 1.5, ... and truncates to the ray index. The bound is defensive — a
    * driver that hands us a larger viewport than we asked for must not scribble
    * past the results buffer. */
   uint idx = uint(gl_FragCoord.x);
   if (idx >= outbuf.r.length()) {
      outColor = vec4(1.0, 0.0, 1.0, 1.0);
      return;
   }

   vec3  origin = vec3(0.25 + float(idx), 0.25, 0.0);
   vec3  dir    = vec3(0.0, 0.0, 1.0);

   rayQueryEXT rq;
   rayQueryInitializeEXT(rq, tlas, params.rayFlags, 0xFF, origin, 0.001,
                         dir, 100.0);

   uint seen = 0u;
   while (rayQueryProceedEXT(rq)) {
      /* OpaqueEXT means Metal commits triangle candidates for us, but a driver
       * is still allowed to surface them here, so handle both. */
      if (rayQueryGetIntersectionTypeEXT(rq, false) ==
          gl_RayQueryCandidateIntersectionTriangleEXT) {
         rayQueryConfirmIntersectionEXT(rq);
      }
      seen += 1u;
   }

   Result res;
   res.committed_type  = rayQueryGetIntersectionTypeEXT(rq, true);
   res.t               = rayQueryGetIntersectionTEXT(rq, true);
   res.prim            = uint(rayQueryGetIntersectionPrimitiveIndexEXT(rq, true));
   res.geom            = uint(rayQueryGetIntersectionGeometryIndexEXT(rq, true));
   res.instance_id     = uint(rayQueryGetIntersectionInstanceIdEXT(rq, true));
   res.custom_index    = uint(rayQueryGetIntersectionInstanceCustomIndexEXT(rq, true));
   vec2 bary           = rayQueryGetIntersectionBarycentricsEXT(rq, true);
   res.bary_x          = bary.x;
   res.bary_y          = bary.y;
   res.front_face      = uint(rayQueryGetIntersectionFrontFaceEXT(rq, true));
   res.candidates_seen = seen;
   res.ray_tmin        = rayQueryGetRayTMinEXT(rq);
   res.ray_flags       = rayQueryGetRayFlagsEXT(rq);

   /* A miss leaves the committed accessors undefined per spec, so zero them
    * rather than reading garbage back into the comparison. */
   if (res.committed_type == gl_RayQueryCommittedIntersectionNoneEXT) {
      res.t = 0.0; res.prim = 0u; res.geom = 0u;
      res.instance_id = 0u; res.custom_index = 0u;
      res.bary_x = 0.0; res.bary_y = 0.0; res.front_face = 0u;
   }

   outbuf.r[idx] = res;

   /* Shade the fragment the way the application does — lit where the shadow ray
    * missed, dark where it hit — so that a screenshot of this probe would show
    * the same thing the app's floor does. */
   float lit = res.committed_type == 0u ? 1.0 : 0.25;
   outColor = vec4(lit, lit, lit, 1.0);
}
