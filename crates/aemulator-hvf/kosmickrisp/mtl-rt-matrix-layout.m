/* mtl-rt-matrix-layout.m — does MTLMatrixLayoutRowMajor do anything on this GPU?
 *
 * Pure Metal, no Vulkan and no KosmicKrisp. It exists because the private
 * -[MTLDevice supportsRayTracingMatrixLayout] answers NO on an Apple M2, and the
 * obvious reading of that NO — "MTLMatrixLayoutRowMajor is silently ignored, so
 * kk's row-major transforms are wrong for everything except the identity" — is
 * measurably false. This is the measurement.
 *
 * kk asks Metal for MTLMatrixLayoutRowMajor for both instance and per-triangle
 * transforms, so that a Vulkan VkTransformMatrixKHR (row-major float[3][4])
 * copies across verbatim instead of being transposed into Metal's historical
 * MTLPackedFloat4x3 (column-major, 4 columns of 3). Identity is its own
 * transpose, which is why a smoke test with identity transforms cannot tell the
 * two apart. So this uses a NON-identity transform: translate +2 along x.
 *
 * Everything is built with the LEGACY MTLAccelerationStructureCommandEncoder,
 * the only builder that intersects at all on an Apple8 (see mtl4-rt-matrix.m), so
 * that a miss here is about the matrix and nothing else.
 *
 * Geometry: one triangle (0,0,1) (4,0,1) (0,4,1), i.e. x+y < 4 at z = 1.
 * Rays: ray0 from x=0.25, ray1 from x=2.25, both at y=0.25 down +z. Untransformed
 * both are inside the triangle. Translated +2 in x, ray0 lands at local x=-1.75
 * (outside) and ray1 at local x=0.25 (inside). "miss,hit" is therefore the
 * signature of a transform that was applied as intended.
 *
 * MEASURED, Apple M2 / macOS 27.0 (26A5388g):
 *
 *   supportsRayTracingMatrixLayout: NO
 *   identity instance (col)             hit,hit    control
 *   instance col-major bytes + Col      miss,hit   correct
 *   instance row-major bytes + Row      miss,hit   correct
 *   instance col-major bytes + Row      hit,hit    differs from + Col
 *   triangle col-major bytes + Col      miss,hit   correct
 *   triangle row-major bytes + Row      miss,hit   correct
 *   triangle col-major bytes + Row      miss,miss  differs from + Col
 *
 * Rows 3 and 6 are the discriminator: feeding the SAME bytes and only changing
 * the declared layout changes the result, so the declaration is honoured, not
 * ignored. Rows 2 and 5 show that honouring it gives the right answer. Whatever
 * -supportsRayTracingMatrixLayout means on this device, it does not mean "row
 * major does nothing", so kk's row-major choice stands and nothing is gated on
 * that selector. See kk_ray_tracing.c.
 *
 * Build and run:
 *   clang -fobjc-arc -O1 -o /tmp/mtl-rt-matrix-layout mtl-rt-matrix-layout.m \
 *       -framework Metal -framework Foundation && /tmp/mtl-rt-matrix-layout
 * or ./run-rq-smoke.sh --matrix, which builds and runs both probes.
 */
#import <Metal/Metal.h>
#import <Foundation/Foundation.h>
#include <objc/message.h>
#include <stdio.h>
#include <string.h>

static const char *kernel_src =
   "#include <metal_stdlib>\n"
   "#include <metal_raytracing>\n"
   "using namespace metal;\n"
   "struct Out { uint type; float t; };\n"
   "kernel void trace(device Out *out [[buffer(0)]],\n"
   "                  device const ulong *ashandle [[buffer(1)]],\n"
   "                  constant float *xs [[buffer(2)]],\n"
   "                  uint i [[thread_position_in_grid]])\n"
   "{\n"
   "  raytracing::acceleration_structure<raytracing::instancing> as =\n"
   "      *(const device raytracing::acceleration_structure<raytracing::instancing> *)ashandle;\n"
   "  raytracing::intersection_query<raytracing::instancing, raytracing::triangle_data> q;\n"
   "  raytracing::intersection_params p;\n"
   "  p.force_opacity(raytracing::forced_opacity::opaque);\n"
   "  q.reset(raytracing::ray(float3(xs[i], 0.25f, 0.0f),\n"
   "                          float3(0,0,1), 0.001f, 100.0f), as, 0xFF, p);\n"
   "  while (q.next()) {}\n"
   "  out[i].type = uint(q.get_committed_intersection_type());\n"
   "  out[i].t    = q.get_committed_distance();\n"
   "}\n";

struct Out {
   uint32_t type;
   float t;
};

/* MTLIndirectAccelerationStructureInstanceDescriptor */
struct indirect_instance {
   float transform[12];
   uint32_t options, mask, ift_offset, user_id;
   uint64_t as_id;
};

#define NRAYS 2

static id<MTLDevice> dev;
static id<MTLCommandQueue> queue;
static id<MTLComputePipelineState> pso;
static id<MTLBuffer> vb, out, ashandle, xsb;
/* Metal does not retain the resources an acceleration-structure build or a trace
 * reads, and ARC would release them at the end of the function that made them. */
static NSMutableArray *keep;

/* Translate +2 along x, in each of the two layouts.
 * MTLPackedFloat4x3 is 4 columns of 3 floats: translation is column 3.
 * VkTransformMatrixKHR is 3 rows of 4 floats: translation is column 3 of a row. */
static const float col_major_tx2[12] = {1, 0, 0, 0, 1, 0, 0, 0, 1, 2, 0, 0};
static const float row_major_tx2[12] = {1, 0, 0, 2, 0, 1, 0, 0, 0, 0, 1, 0};
static const float identity_col[12] = {1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0};

static void
legacy_build(id<MTLAccelerationStructure> as,
             MTLAccelerationStructureDescriptor *d)
{
   MTLAccelerationStructureSizes s =
      [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLBuffer> scratch =
      [dev newBufferWithLength:s.buildScratchBufferSize ?: 256
                      options:MTLResourceStorageModeShared];
   [keep addObject:scratch];
   id<MTLCommandBuffer> cb = [queue commandBuffer];
   id<MTLAccelerationStructureCommandEncoder> enc =
      [cb accelerationStructureCommandEncoder];
   [enc buildAccelerationStructure:as
                       descriptor:d
                    scratchBuffer:scratch
              scratchBufferOffset:0];
   [enc endEncoding];
   [cb commit];
   [cb waitUntilCompleted];
}

static id<MTLAccelerationStructure>
blas(const float *tx, MTLMatrixLayout layout, bool have_tx)
{
   MTLAccelerationStructureTriangleGeometryDescriptor *g =
      [MTLAccelerationStructureTriangleGeometryDescriptor descriptor];
   g.vertexBuffer = vb;
   g.vertexStride = 12;
   g.triangleCount = 1;
   g.opaque = YES;
   if (have_tx) {
      id<MTLBuffer> tb = [dev newBufferWithBytes:tx
                                         length:48
                                        options:MTLResourceStorageModeShared];
      [keep addObject:tb];
      g.transformationMatrixBuffer = tb;
      g.transformationMatrixLayout = layout;
   }
   MTLPrimitiveAccelerationStructureDescriptor *d =
      [MTLPrimitiveAccelerationStructureDescriptor descriptor];
   d.geometryDescriptors = @[ g ];
   MTLAccelerationStructureSizes s =
      [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLAccelerationStructure> as =
      [dev newAccelerationStructureWithSize:s.accelerationStructureSize];
   [keep addObject:as];
   legacy_build(as, d);
   return as;
}

static id<MTLAccelerationStructure>
tlas(id<MTLAccelerationStructure> b, const float *tx, MTLMatrixLayout layout)
{
   struct indirect_instance ii = {
      .options = MTLAccelerationStructureInstanceOptionDisableTriangleCulling,
      .mask = 0xFF,
      .user_id = 0x123456,
      .as_id = b.gpuResourceID._impl,
   };
   memcpy(ii.transform, tx, sizeof(ii.transform));
   id<MTLBuffer> ib = [dev newBufferWithBytes:&ii
                                      length:sizeof(ii)
                                     options:MTLResourceStorageModeShared];
   uint32_t count = 1;
   id<MTLBuffer> cntb = [dev newBufferWithBytes:&count
                                        length:sizeof(count)
                                       options:MTLResourceStorageModeShared];
   [keep addObject:ib];
   [keep addObject:cntb];

   MTLIndirectInstanceAccelerationStructureDescriptor *d =
      [MTLIndirectInstanceAccelerationStructureDescriptor descriptor];
   d.instanceDescriptorBuffer = ib;
   d.instanceDescriptorStride = sizeof(struct indirect_instance);
   d.maxInstanceCount = 1;
   d.instanceCountBuffer = cntb;
   d.instanceDescriptorType =
      MTLAccelerationStructureInstanceDescriptorTypeIndirect;
   d.instanceTransformationMatrixLayout = layout;

   MTLAccelerationStructureSizes s =
      [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLAccelerationStructure> as =
      [dev newAccelerationStructureWithSize:s.accelerationStructureSize];
   [keep addObject:as];
   legacy_build(as, d);
   return as;
}

/* Trace both rays and return the "miss,hit"-style signature. */
static const char *
trace(const char *label, id<MTLAccelerationStructure> t)
{
   static char sig[32];
   memset(out.contents, 0, NRAYS * sizeof(struct Out));
   *(uint64_t *)ashandle.contents = t.gpuResourceID._impl;

   id<MTLCommandBuffer> cb = [queue commandBuffer];
   id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
   [enc setComputePipelineState:pso];
   [enc setBuffer:out offset:0 atIndex:0];
   [enc setBuffer:ashandle offset:0 atIndex:1];
   [enc setBuffer:xsb offset:0 atIndex:2];
   [enc useResource:t usage:MTLResourceUsageRead];
   [enc dispatchThreads:MTLSizeMake(NRAYS, 1, 1)
      threadsPerThreadgroup:MTLSizeMake(NRAYS, 1, 1)];
   [enc endEncoding];
   [cb commit];
   [cb waitUntilCompleted];

   const struct Out *o = out.contents;
   snprintf(sig, sizeof(sig), "%s,%s", o[0].type == 1 ? "hit" : "miss",
            o[1].type == 1 ? "hit" : "miss");
   printf("  %-32s ray0(x=0.25) type=%u t=%-7.3f ray1(x=2.25) type=%u t=%-7.3f "
          "=> %s\n",
          label, o[0].type, o[0].t, o[1].type, o[1].t, sig);
   return sig;
}

int
main(void)
{
   @autoreleasepool {
      keep = [NSMutableArray new];
      dev = MTLCreateSystemDefaultDevice();
      if (dev == nil) {
         printf("no Metal device\n");
         return 1;
      }
      queue = [dev newCommandQueue];
      printf("device: %s\n", dev.name.UTF8String);

      SEL mlay = NSSelectorFromString(@"supportsRayTracingMatrixLayout");
      if ([dev respondsToSelector:mlay]) {
         printf("private -supportsRayTracingMatrixLayout: %s\n",
                ((BOOL (*)(id, SEL))objc_msgSend)(dev, mlay) ? "YES" : "NO");
      } else {
         printf("private -supportsRayTracingMatrixLayout: selector absent\n");
      }

      NSError *err = nil;
      id<MTLLibrary> lib =
         [dev newLibraryWithSource:[NSString stringWithUTF8String:kernel_src]
                          options:nil
                            error:&err];
      if (lib == nil) {
         printf("library: %s\n", err.description.UTF8String);
         return 1;
      }
      pso = [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"trace"]
                                             error:&err];
      if (pso == nil) {
         printf("pipeline: %s\n", err.description.UTF8String);
         return 1;
      }

      static const float verts[9] = {0, 0, 1, 4, 0, 1, 0, 4, 1};
      vb = [dev newBufferWithBytes:verts
                           length:sizeof(verts)
                          options:MTLResourceStorageModeShared];
      out = [dev newBufferWithLength:NRAYS * sizeof(struct Out)
                            options:MTLResourceStorageModeShared];
      ashandle =
         [dev newBufferWithLength:8 options:MTLResourceStorageModeShared];
      static const float xs[NRAYS] = {0.25f, 2.25f};
      xsb = [dev newBufferWithBytes:xs
                            length:sizeof(xs)
                           options:MTLResourceStorageModeShared];

      printf("\n--- control: no transform anywhere (both rays must hit) ---\n");
      id<MTLAccelerationStructure> plain =
         blas(NULL, MTLMatrixLayoutColumnMajor, false);
      /* trace() hands back a pointer into one static buffer, so every signature
       * that outlives the next trace has to be copied out of it. */
      char control[32];
      snprintf(control, sizeof(control), "%s",
               trace("identity instance (col)",
                     tlas(plain, identity_col, MTLMatrixLayoutColumnMajor)));

      printf("\n--- INSTANCE transform, translate +2x (want miss,hit) ---\n");
      char i_col[32], i_row[32], i_mixed[32];
      snprintf(i_col, sizeof(i_col), "%s",
               trace("instance col-major bytes + Col",
                     tlas(plain, col_major_tx2, MTLMatrixLayoutColumnMajor)));
      snprintf(i_row, sizeof(i_row), "%s",
               trace("instance row-major bytes + Row",
                     tlas(plain, row_major_tx2, MTLMatrixLayoutRowMajor)));
      snprintf(i_mixed, sizeof(i_mixed), "%s",
               trace("instance col-major bytes + Row",
                     tlas(plain, col_major_tx2, MTLMatrixLayoutRowMajor)));

      printf("\n--- TRIANGLE transform, translate +2x (want miss,hit) ---\n");
      char t_col[32], t_row[32], t_mixed[32];
      snprintf(t_col, sizeof(t_col), "%s",
               trace("triangle col-major bytes + Col",
                     tlas(blas(col_major_tx2, MTLMatrixLayoutColumnMajor, true),
                          identity_col, MTLMatrixLayoutColumnMajor)));
      snprintf(t_row, sizeof(t_row), "%s",
               trace("triangle row-major bytes + Row",
                     tlas(blas(row_major_tx2, MTLMatrixLayoutRowMajor, true),
                          identity_col, MTLMatrixLayoutColumnMajor)));
      snprintf(t_mixed, sizeof(t_mixed), "%s",
               trace("triangle col-major bytes + Row",
                     tlas(blas(col_major_tx2, MTLMatrixLayoutRowMajor, true),
                          identity_col, MTLMatrixLayoutColumnMajor)));

      printf("\n--- verdict ---\n");
      if (strcmp(control, "hit,hit") != 0) {
         printf("  the control did not hit (%s) — nothing below means anything;\n"
                "  this GPU cannot intersect even a legacy-built structure.\n",
                control);
         return 1;
      }
      const bool i_ok = strcmp(i_row, "miss,hit") == 0;
      const bool t_ok = strcmp(t_row, "miss,hit") == 0;
      const bool i_honoured = strcmp(i_mixed, i_col) != 0;
      const bool t_honoured = strcmp(t_mixed, t_col) != 0;
      printf("  instance MTLMatrixLayoutRowMajor: %s, and %s\n",
             i_honoured ? "HONOURED (same bytes, other layout, other result)"
                        : "IGNORED (layout made no difference)",
             i_ok ? "row-major bytes transform correctly"
                  : "row-major bytes DO NOT transform correctly");
      printf("  triangle MTLMatrixLayoutRowMajor: %s, and %s\n",
             t_honoured ? "HONOURED (same bytes, other layout, other result)"
                        : "IGNORED (layout made no difference)",
             t_ok ? "row-major bytes transform correctly"
                  : "row-major bytes DO NOT transform correctly");
      if (i_honoured && i_ok && t_honoured && t_ok) {
         printf("  => kk may keep MTL_MATRIX_LAYOUT_ROW_MAJOR on this device.\n");
         return 0;
      }
      printf("  => kk must NOT rely on MTL_MATRIX_LAYOUT_ROW_MAJOR here: it has\n"
             "     to transpose VkTransformMatrixKHR into MTLPackedFloat4x3\n"
             "     (out[c*3+r] = in[r*4+c]) and declare column-major instead.\n");
      return 1;
   }
}
