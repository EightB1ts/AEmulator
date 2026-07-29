/* mtl4-rt-matrix.m — which Metal ray-tracing build path traces on THIS GPU?
 *
 * Pure Metal, no Vulkan and no KosmicKrisp: this is the control for a partial
 * pass out of run-rq-smoke.sh. It builds the same one-triangle BLAS and its
 * one-instance TLAS through either the legacy MTLAccelerationStructureCommandEncoder
 * or the Metal 4 compute encoder, in every combination, then traces each result
 * from both a legacy and a Metal 4 dispatch. One line per combination.
 *
 * KosmicKrisp is a Metal 4 driver, so MTL4/MTL4 is the row that decides whether
 * the driver can ever return a hit. Measured on an Apple M2 / macOS 27.0
 * (26A5388g): only legacy-built structures intersect, from either encoder, and
 * no MTL4 variant does — not with the sizes Metal asks for, not oversized, not
 * from a legacy descriptor handed to the MTL4 encoder, not from a structure
 * allocated by descriptor. The build raises no error and no commit feedback.
 *
 * Re-run under Metal API validation for the one-line reason:
 *
 *    METAL_DEVICE_WRAPPER_TYPE=1 ./mtl4-rt-matrix
 *    _validateRTDescriptorCompatibility: failed assertion
 *    `Acceleration Structure Descriptor Validation
 *     this device does not support Metal 4 ray tracing.'
 *
 * Nothing public reports that: supportsRaytracing and
 * supportsFamily:MTLGPUFamilyMetal4 are both YES on the M2. Hardware ray
 * tracing arrives with Apple9, and below it Metal emulates ray tracing with a
 * builder only the legacy encoder can reach. */
#import <Metal/Metal.h>
#import <Metal/MTL4AccelerationStructure.h>
#import <Metal/MTL4CommandQueue.h>
#import <Metal/MTL4CommandBuffer.h>
#import <Metal/MTL4ComputeCommandEncoder.h>
#import <Metal/MTL4CommitFeedback.h>
#import <Foundation/Foundation.h>
#include <stdio.h>

static const char *kernel_src =
   "#include <metal_stdlib>\n"
   "#include <metal_raytracing>\n"
   "using namespace metal;\n"
   "struct Out { uint type; float t; uint custom; float bx; float by; };\n"
   "kernel void trace(device Out *out [[buffer(0)]],\n"
   "                  device const ulong *ashandle [[buffer(1)]],\n"
   "                  uint i [[thread_position_in_grid]])\n"
   "{\n"
   "  raytracing::acceleration_structure<raytracing::instancing> as =\n"
   "      *(const device raytracing::acceleration_structure<raytracing::instancing> *)ashandle;\n"
   "  raytracing::intersection_query<raytracing::instancing, raytracing::triangle_data> q;\n"
   "  raytracing::intersection_params p;\n"
   "  p.force_opacity(raytracing::forced_opacity::opaque);\n"
   "  q.reset(raytracing::ray(float3(0.25f + float(i), 0.25f, 0.0f),\n"
   "                          float3(0,0,1), 0.001f, 100.0f), as, 0xFF, p);\n"
   "  while (q.next()) {}\n"
   "  out[i].type   = uint(q.get_committed_intersection_type());\n"
   "  out[i].t      = q.get_committed_distance();\n"
   "  out[i].custom = q.get_committed_user_instance_id();\n"
   "  float2 b      = q.get_committed_triangle_barycentric_coord();\n"
   "  out[i].bx = b.x; out[i].by = b.y;\n"
   "}\n";

struct Out { uint32_t type; float t; uint32_t custom; float bx; float by; };
struct indirect_instance {
   float transform[4][3];
   uint32_t options, mask, ift_offset, user_id;
   uint64_t as_id;
};

#define NRAYS 2

static id<MTLDevice> dev;
static id<MTLCommandQueue> q3;
static id<MTL4CommandQueue> q4;
static id<MTLResidencySet> rs;
static id<MTLComputePipelineState> pso;
static id<MTLBuffer> vb, out, ashandle;

static void
resident(id r)
{
   [rs addAllocation:r];
   [rs commit];
}

/* Submit one MTL4 command buffer holding whatever `body` encodes. */
static void
mtl4_submit(void (^body)(id<MTL4ComputeCommandEncoder> enc))
{
   id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
   id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
   [cb beginCommandBufferWithAllocator:alloc];
   id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
   body(enc);
   [enc endEncoding];
   [cb endCommandBuffer];
   MTL4CommitOptions *co = [MTL4CommitOptions new];
   dispatch_semaphore_t sem = dispatch_semaphore_create(0);
   [co addFeedbackHandler:^(id<MTL4CommitFeedback> fb) {
      if (fb.error)
         printf("    MTL4 commit error: %s\n", fb.error.description.UTF8String);
      dispatch_semaphore_signal(sem);
   }];
   [q4 commit:&cb count:1 options:co];
   dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
}

static void
mtl3_build(id<MTLAccelerationStructure> as, MTLAccelerationStructureDescriptor *d,
           id<MTLBuffer> scratch)
{
   id<MTLCommandBuffer> cb = [q3 commandBuffer];
   id<MTLAccelerationStructureCommandEncoder> enc =
      [cb accelerationStructureCommandEncoder];
   [enc buildAccelerationStructure:as descriptor:d scratchBuffer:scratch
              scratchBufferOffset:0];
   [enc endEncoding];
   [cb commit];
   [cb waitUntilCompleted];
}

/* ---- BLAS ---- */

static id<MTLAccelerationStructure>
blas_mtl3(void)
{
   MTLAccelerationStructureTriangleGeometryDescriptor *g =
      [MTLAccelerationStructureTriangleGeometryDescriptor descriptor];
   g.vertexBuffer = vb; g.vertexStride = 12; g.triangleCount = 1; g.opaque = YES;
   MTLPrimitiveAccelerationStructureDescriptor *d =
      [MTLPrimitiveAccelerationStructureDescriptor descriptor];
   d.geometryDescriptors = @[ g ];
   MTLAccelerationStructureSizes s = [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLAccelerationStructure> as =
      [dev newAccelerationStructureWithSize:s.accelerationStructureSize];
   id<MTLBuffer> scratch = [dev newBufferWithLength:s.buildScratchBufferSize ?: 256
                                           options:MTLResourceStorageModeShared];
   resident(as);
   mtl3_build(as, d, scratch);
   return as;
}

static id<MTLAccelerationStructure>
blas_mtl4(void)
{
   MTL4AccelerationStructureTriangleGeometryDescriptor *g =
      [MTL4AccelerationStructureTriangleGeometryDescriptor new];
   g.vertexBuffer = MTL4BufferRangeMake(vb.gpuAddress, vb.length);
   g.vertexFormat = MTLAttributeFormatFloat3;
   g.vertexStride = 12; g.triangleCount = 1; g.opaque = YES;
   MTL4PrimitiveAccelerationStructureDescriptor *d =
      [MTL4PrimitiveAccelerationStructureDescriptor new];
   d.geometryDescriptors = @[ g ];
   MTLAccelerationStructureSizes s = [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLAccelerationStructure> as =
      [dev newAccelerationStructureWithSize:s.accelerationStructureSize];
   id<MTLBuffer> scratch = [dev newBufferWithLength:s.buildScratchBufferSize ?: 256
                                           options:MTLResourceStorageModeShared];
   resident(as);
   resident(scratch);
   mtl4_submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc buildAccelerationStructure:as descriptor:d
                        scratchBuffer:MTL4BufferRangeMake(scratch.gpuAddress,
                                                          scratch.length)];
   });
   return as;
}

/* Same MTL4 build, but into a deliberately oversized structure and scratch.
 * accelerationStructureSizesWithDescriptor: answers 1280/5632 for the MTL4
 * descriptor and 1536/6400 for the equivalent legacy one, so if the legacy
 * sizing query simply cannot read an MTL4 descriptor, the build has been
 * writing into an allocation that is too small all along. */
static id<MTLAccelerationStructure>
blas_mtl4_oversized(void)
{
   MTL4AccelerationStructureTriangleGeometryDescriptor *g =
      [MTL4AccelerationStructureTriangleGeometryDescriptor new];
   g.vertexBuffer = MTL4BufferRangeMake(vb.gpuAddress, vb.length);
   g.vertexFormat = MTLAttributeFormatFloat3;
   g.vertexStride = 12; g.triangleCount = 1; g.opaque = YES;
   MTL4PrimitiveAccelerationStructureDescriptor *d =
      [MTL4PrimitiveAccelerationStructureDescriptor new];
   d.geometryDescriptors = @[ g ];
   id<MTLAccelerationStructure> as = [dev newAccelerationStructureWithSize:65536];
   id<MTLBuffer> scratch = [dev newBufferWithLength:65536
                                           options:MTLResourceStorageModeShared];
   resident(as);
   resident(scratch);
   mtl4_submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc buildAccelerationStructure:as descriptor:d
                        scratchBuffer:MTL4BufferRangeMake(scratch.gpuAddress,
                                                          scratch.length)];
   });
   return as;
}

/* MTL4 encoder, LEGACY descriptor. MTL4AccelerationStructureDescriptor derives
 * from MTLAccelerationStructureDescriptor, so the encoder accepts either at the
 * ABI level. If this traces, the MTL4 *descriptor* is what Metal mishandles on
 * this GPU rather than the MTL4 build command. */
static id<MTLAccelerationStructure>
blas_mtl4_legacy_desc(void)
{
   MTLAccelerationStructureTriangleGeometryDescriptor *g =
      [MTLAccelerationStructureTriangleGeometryDescriptor descriptor];
   g.vertexBuffer = vb; g.vertexStride = 12; g.triangleCount = 1; g.opaque = YES;
   MTLPrimitiveAccelerationStructureDescriptor *d =
      [MTLPrimitiveAccelerationStructureDescriptor descriptor];
   d.geometryDescriptors = @[ g ];
   MTLAccelerationStructureSizes s = [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLAccelerationStructure> as =
      [dev newAccelerationStructureWithSize:s.accelerationStructureSize];
   id<MTLBuffer> scratch = [dev newBufferWithLength:s.buildScratchBufferSize ?: 256
                                           options:MTLResourceStorageModeShared];
   resident(as);
   resident(scratch);
   mtl4_submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc buildAccelerationStructure:as
                          descriptor:(MTL4AccelerationStructureDescriptor *)d
                        scratchBuffer:MTL4BufferRangeMake(scratch.gpuAddress,
                                                          scratch.length)];
   });
   return as;
}

/* MTL4 build into a structure ALLOCATED FROM THE DESCRIPTOR rather than from a
 * byte size, in case Metal has to know the BVH format at allocation time. */
static id<MTLAccelerationStructure>
blas_mtl4_asdesc(void)
{
   MTL4AccelerationStructureTriangleGeometryDescriptor *g =
      [MTL4AccelerationStructureTriangleGeometryDescriptor new];
   g.vertexBuffer = MTL4BufferRangeMake(vb.gpuAddress, vb.length);
   g.vertexFormat = MTLAttributeFormatFloat3;
   g.vertexStride = 12; g.triangleCount = 1; g.opaque = YES;
   MTL4PrimitiveAccelerationStructureDescriptor *d =
      [MTL4PrimitiveAccelerationStructureDescriptor new];
   d.geometryDescriptors = @[ g ];
   id<MTLAccelerationStructure> as = [dev newAccelerationStructureWithDescriptor:d];
   if (!as) { printf("  newAccelerationStructureWithDescriptor: nil\n"); return nil; }
   MTLAccelerationStructureSizes s = [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLBuffer> scratch = [dev newBufferWithLength:s.buildScratchBufferSize ?: 256
                                           options:MTLResourceStorageModeShared];
   resident(as);
   resident(scratch);
   mtl4_submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc buildAccelerationStructure:as descriptor:d
                        scratchBuffer:MTL4BufferRangeMake(scratch.gpuAddress,
                                                          scratch.length)];
   });
   return as;
}

/* ---- TLAS ---- */

static id<MTLBuffer>
instance_buffer(id<MTLAccelerationStructure> blas)
{
   struct indirect_instance ii = {
      .transform = {{1, 0, 0}, {0, 1, 0}, {0, 0, 1}, {0, 0, 0}},
      .options = MTLAccelerationStructureInstanceOptionDisableTriangleCulling,
      .mask = 0xFF,
      .user_id = 0x123456,
      .as_id = blas.gpuResourceID._impl,
   };
   id<MTLBuffer> ib = [dev newBufferWithBytes:&ii length:sizeof(ii)
                                      options:MTLResourceStorageModeShared];
   resident(ib);
   return ib;
}

static id<MTLAccelerationStructure>
tlas_mtl3(id<MTLAccelerationStructure> blas)
{
   id<MTLBuffer> ib = instance_buffer(blas);
   MTLIndirectInstanceAccelerationStructureDescriptor *d =
      [MTLIndirectInstanceAccelerationStructureDescriptor descriptor];
   uint32_t count = 1;
   id<MTLBuffer> cntb = [dev newBufferWithBytes:&count length:4
                                       options:MTLResourceStorageModeShared];
   d.instanceDescriptorBuffer = ib;
   d.instanceDescriptorStride = sizeof(struct indirect_instance);
   d.maxInstanceCount = 1;
   d.instanceCountBuffer = cntb;
   d.instanceDescriptorType = MTLAccelerationStructureInstanceDescriptorTypeIndirect;
   MTLAccelerationStructureSizes s = [dev accelerationStructureSizesWithDescriptor:d];
   id<MTLAccelerationStructure> as =
      [dev newAccelerationStructureWithSize:s.accelerationStructureSize];
   id<MTLBuffer> scratch = [dev newBufferWithLength:s.buildScratchBufferSize ?: 256
                                           options:MTLResourceStorageModeShared];
   resident(as);
   mtl3_build(as, d, scratch);
   return as;
}

static id<MTLAccelerationStructure>
tlas_mtl4(id<MTLAccelerationStructure> blas)
{
   id<MTLBuffer> ib = instance_buffer(blas);
   MTL4InstanceAccelerationStructureDescriptor *d =
      [MTL4InstanceAccelerationStructureDescriptor new];
   d.instanceDescriptorBuffer =
      MTL4BufferRangeMake(ib.gpuAddress, sizeof(struct indirect_instance));
   d.instanceDescriptorStride = sizeof(struct indirect_instance);
   d.instanceCount = 1;
   d.instanceDescriptorType = MTLAccelerationStructureInstanceDescriptorTypeIndirect;
   d.instanceTransformationMatrixLayout = MTLMatrixLayoutColumnMajor;

   /* accelerationStructureSizesWithDescriptor: throws on an MTL4 instance
    * descriptor (it reaches for -motionKeyframeCount), so size with the legacy
    * indirect descriptor the way the driver does. */
   MTLIndirectInstanceAccelerationStructureDescriptor *sizer =
      [MTLIndirectInstanceAccelerationStructureDescriptor descriptor];
   sizer.instanceDescriptorStride = sizeof(struct indirect_instance);
   sizer.maxInstanceCount = 1;
   sizer.instanceDescriptorType = MTLAccelerationStructureInstanceDescriptorTypeIndirect;
   MTLAccelerationStructureSizes s =
      [dev accelerationStructureSizesWithDescriptor:sizer];

   id<MTLAccelerationStructure> as =
      [dev newAccelerationStructureWithSize:s.accelerationStructureSize];
   id<MTLBuffer> scratch = [dev newBufferWithLength:s.buildScratchBufferSize ?: 256
                                           options:MTLResourceStorageModeShared];
   resident(as);
   resident(scratch);
   mtl4_submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc buildAccelerationStructure:as descriptor:d
                        scratchBuffer:MTL4BufferRangeMake(scratch.gpuAddress,
                                                          scratch.length)];
   });
   return as;
}

/* ---- trace ---- */

static void
trace(const char *label, id<MTLAccelerationStructure> tlas)
{
   memset(out.contents, 0, NRAYS * sizeof(struct Out));
   *(uint64_t *)ashandle.contents = tlas.gpuResourceID._impl;
   id<MTLCommandBuffer> cb = [q3 commandBuffer];
   id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
   [enc setComputePipelineState:pso];
   [enc setBuffer:out offset:0 atIndex:0];
   [enc setBuffer:ashandle offset:0 atIndex:1];
   [enc dispatchThreads:MTLSizeMake(NRAYS, 1, 1)
     threadsPerThreadgroup:MTLSizeMake(NRAYS, 1, 1)];
   [enc endEncoding];
   [cb commit];
   [cb waitUntilCompleted];
   const struct Out *o = out.contents;
   printf("  %-18s ray0 type=%u t=%.4f custom=0x%x bary=(%.3f,%.3f)  ray1 type=%u  => %s\n",
          label, o[0].type, o[0].t, o[0].custom, o[0].bx, o[0].by, o[1].type,
          (o[0].type == 1 && o[1].type == 0) ? "HIT+MISS" : "no hit");
}

/* Same trace, but issued from the Metal 4 compute encoder — which is the one
 * KosmicKrisp dispatches on. If an MTL4-built structure only traces from an
 * MTL4 encoder, the legacy-trace results above would be a red herring. */
static void
trace4(const char *label, id<MTLAccelerationStructure> tlas)
{
   memset(out.contents, 0, NRAYS * sizeof(struct Out));
   *(uint64_t *)ashandle.contents = tlas.gpuResourceID._impl;

   MTL4ComputePipelineDescriptor *pd = [MTL4ComputePipelineDescriptor new];
   MTL4LibraryFunctionDescriptor *fd = [MTL4LibraryFunctionDescriptor new];
   NSError *err = nil;
   id<MTL4Compiler> compiler =
      [dev newCompilerWithDescriptor:[MTL4CompilerDescriptor new] error:&err];
   MTL4LibraryDescriptor *ld = [MTL4LibraryDescriptor new];
   ld.source = [NSString stringWithUTF8String:kernel_src];
   id<MTLLibrary> lib = [compiler newLibraryWithDescriptor:ld error:&err];
   if (!lib) { printf("  %-18s MTL4 library: %s\n", label, err.description.UTF8String); return; }
   fd.name = @"trace";
   fd.library = lib;
   pd.computeFunctionDescriptor = fd;
   id<MTLComputePipelineState> p4 =
      [compiler newComputePipelineStateWithDescriptor:pd
                             compilerTaskOptions:nil error:&err];
   if (!p4) { printf("  %-18s MTL4 pso: %s\n", label, err.description.UTF8String); return; }

   MTL4ArgumentTableDescriptor *atd = [MTL4ArgumentTableDescriptor new];
   atd.maxBufferBindCount = 2;
   id<MTL4ArgumentTable> at = [dev newArgumentTableWithDescriptor:atd error:&err];
   [at setAddress:out.gpuAddress atIndex:0];
   [at setAddress:ashandle.gpuAddress atIndex:1];

   mtl4_submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc setArgumentTable:at];
      [enc setComputePipelineState:p4];
      [enc dispatchThreads:MTLSizeMake(NRAYS, 1, 1)
            threadsPerThreadgroup:MTLSizeMake(NRAYS, 1, 1)];
   });

   const struct Out *o = out.contents;
   printf("  %-18s ray0 type=%u t=%.4f custom=0x%x bary=(%.3f,%.3f)  ray1 type=%u  => %s\n",
          label, o[0].type, o[0].t, o[0].custom, o[0].bx, o[0].by, o[1].type,
          (o[0].type == 1 && o[1].type == 0) ? "HIT+MISS" : "no hit");
}

int
main(void)
{
   @autoreleasepool {
      dev = MTLCreateSystemDefaultDevice();
      q3 = [dev newCommandQueue];
      q4 = [dev newMTL4CommandQueue];
      printf("device: %s\n", dev.name.UTF8String);

      MTLResidencySetDescriptor *rsd = [MTLResidencySetDescriptor new];
      rs = [dev newResidencySetWithDescriptor:rsd error:nil];
      [q4 addResidencySet:rs];
      [q3 addResidencySet:rs];

      NSError *err = nil;
      id<MTLLibrary> lib =
         [dev newLibraryWithSource:[NSString stringWithUTF8String:kernel_src]
                           options:nil error:&err];
      if (!lib) { printf("shader: %s\n", err.description.UTF8String); return 1; }
      pso = [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"trace"]
                                              error:&err];

      float verts[3][3] = {{0, 0, 1}, {1, 0, 1}, {0, 1, 1}};
      vb = [dev newBufferWithBytes:verts length:sizeof(verts)
                          options:MTLResourceStorageModeShared];
      out = [dev newBufferWithLength:NRAYS * sizeof(struct Out)
                            options:MTLResourceStorageModeShared];
      ashandle = [dev newBufferWithLength:8 options:MTLResourceStorageModeShared];
      resident(vb);
      resident(out);
      resident(ashandle);

      id<MTLAccelerationStructure> b3 = blas_mtl3();
      id<MTLAccelerationStructure> b4 = blas_mtl4();

      id<MTLAccelerationStructure> t33 = tlas_mtl3(b3);
      id<MTLAccelerationStructure> t34 = tlas_mtl4(b3);
      id<MTLAccelerationStructure> t43 = tlas_mtl3(b4);
      id<MTLAccelerationStructure> t44 = tlas_mtl4(b4);

      printf("--- traced from the legacy compute encoder ---\n");
      trace("BLAS3+TLAS3", t33);
      trace("BLAS3+TLAS4", t34);
      trace("BLAS4+TLAS3", t43);
      trace("BLAS4+TLAS4", t44);

      id<MTLAccelerationStructure> b4big = blas_mtl4_oversized();
      trace("BLAS4big+TLAS3", tlas_mtl3(b4big));
      id<MTLAccelerationStructure> b4ld = blas_mtl4_legacy_desc();
      trace("BLAS4legacyd+TLAS3", tlas_mtl3(b4ld));
      id<MTLAccelerationStructure> b4ad = blas_mtl4_asdesc();
      if (b4ad) trace("BLAS4fromdesc+TLAS3", tlas_mtl3(b4ad));

      printf("--- traced from the Metal 4 compute encoder ---\n");
      trace4("BLAS3+TLAS3", t33);
      trace4("BLAS3+TLAS4", t34);
      trace4("BLAS4+TLAS3", t43);
      trace4("BLAS4+TLAS4", t44);
   }
   return 0;
}
