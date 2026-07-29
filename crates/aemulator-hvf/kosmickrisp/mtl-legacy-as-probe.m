/* mtl-legacy-as-probe.m — can KosmicKrisp's shapes reach the legacy AS builder?
 *
 * mtl4-rt-matrix.m already established WHICH builder works on an Apple8: only
 * -[MTLCommandBuffer accelerationStructureCommandEncoder], never
 * -[MTL4ComputeCommandEncoder buildAccelerationStructure:...]. That probe used
 * the shapes that are convenient in a standalone file: device-level buffers at
 * offset 0, a device-level acceleration structure, one queue at a time, and a
 * blocking -waitUntilCompleted after every submit.
 *
 * KosmicKrisp cannot use any of those. So this probe answers the four questions
 * that stand between "the legacy builder works" and "the legacy builder works
 * inside this driver", each of which would otherwise be found out the hard way:
 *
 *   1. BUFFER OFFSETS. Every kk buffer is a suballocation of one whole-heap
 *      MTLBuffer, so a legacy descriptor gets that buffer plus a NONZERO offset
 *      rather than a dedicated buffer. Does the legacy builder honour
 *      vertexBufferOffset, indexBufferOffset and scratchBufferOffset?
 *      MEASURED: YES, indexed and non-indexed, ray0 t=1.0 custom=0x123456
 *      bary=(0.250,0.250).
 *
 *   2. instanceCountBuffer. The legacy TLAS descriptor that takes indirect
 *      instance descriptors (MTLResourceID embedded per instance, which is what
 *      kk's conversion kernel writes) is
 *      MTLIndirectInstanceAccelerationStructureDescriptor, and it has a
 *      *maxInstanceCount* plus a nullable instanceCountBuffer holding the real
 *      count. kk knows the count at record time. Must the buffer be supplied
 *      anyway, or does nil mean maxInstanceCount? The answer decides whether kk
 *      has to find four more bytes of GPU-writable scratch per TLAS build.
 *      MEASURED: nil instanceCountBuffer with maxInstanceCount = 1 HITS, so no
 *      count buffer is needed. The non-indirect
 *      MTLInstanceAccelerationStructureDescriptor with instanceDescriptorType =
 *      Indirect and a plain instanceCount also hits; kk uses the indirect one
 *      anyway, because that is the class -accelerationStructureSizesWithDescriptor:
 *      accepts and kk already sizes with it.
 *
 *   3. CROSS-QUEUE ORDERING. kk submits MTL4 command buffers to an
 *      MTL4CommandQueue. A legacy AS encoder comes from a legacy MTLCommandBuffer,
 *      which needs a legacy MTLCommandQueue, so the two have to be ordered against
 *      each other. kk already has MTLSharedEvent for VkSemaphore. Does
 *      MTL4CommandQueue -signalEvent:value:/-waitForEvent:value: interleave
 *      correctly with a legacy command buffer's -encodeWaitForEvent:value:/
 *      -encodeSignalEvent:value: on the same device, with no CPU wait in between?
 *      The realistic pattern is exactly kk's TLAS path: an MTL4 compute dispatch
 *      writes the converted instance array, the legacy encoder builds the TLAS
 *      from it, then an MTL4 compute dispatch traces the result. If the ordering
 *      is not real, that is a race that only shows up under load.
 *      MEASURED: YES. The build reads an instance descriptor that a
 *      still-in-flight MTL4 dispatch produced, and hits. No deadlock, no CPU
 *      wait between the two queues. Note the -encodeWaitForEvent: has to be
 *      called BEFORE the encoders are created, since a legacy command buffer
 *      orders event operations against encoders by call order — which is why kk
 *      allocates one MTLEvent per acceleration-structure pass at RECORD time
 *      with fixed values 1 and 2, instead of one shared counter at submit time.
 *
 *   4. RESIDENCY. kk keeps everything alive with one MTLResidencySet on the
 *      queue. Does adding that same set to the legacy queue make heap-placed
 *      geometry and the destination structure resident for a legacy AS build, or
 *      does the legacy encoder need -useResource:/-useHeap: calls?
 *      MEASURED: the queue-level set plus -[MTLCommandBuffer useResidencySet:],
 *      which is what kk does, is sufficient; no -useResource: anywhere.
 *
 * And one thing this probe found by failing: an acceleration structure CANNOT be
 * heap-placed here. -[MTLHeap newAccelerationStructureWithSize:offset:] returns
 * nil on a placement heap whose storageMode is Shared and succeeds only on a
 * Private one (measured across five heap configurations). Every kk heap is
 * Shared — KK_MTL_RESOURCE_OPTIONS in kk_private.h — so
 * kk_CreateAccelerationStructureKHR's attempt to place the structure inside the
 * application's VkBuffer always fails on this device and always falls through to
 * the standalone -[MTLDevice newAccelerationStructureWithSize:] plus a
 * residency-set entry. That is the shape reproduced below.
 *
 * Every check prints its own verdict. A hit is read back from the buffer, never
 * inferred from the absence of an error.
 *
 *   clang -fobjc-arc -O1 -o /tmp/mtl-legacy-as-probe mtl-legacy-as-probe.m \
 *       -framework Metal -framework Foundation && /tmp/mtl-legacy-as-probe
 * or ./run-rq-smoke.sh --matrix, which builds and runs it with the other probes.
 */
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <stdio.h>
#include <string.h>

static const char *kernel_src =
   "#include <metal_stdlib>\n"
   "#include <metal_raytracing>\n"
   "using namespace metal;\n"
   "struct Out { uint type; float t; uint custom; float bx; float by; };\n"
   "kernel void trace(device Out *out [[buffer(0)]],\n"
   "                  device const ulong *ash [[buffer(1)]],\n"
   "                  uint i [[thread_position_in_grid]]) {\n"
   "  raytracing::acceleration_structure<raytracing::instancing> as =\n"
   "      *(const device raytracing::acceleration_structure<\n"
   "            raytracing::instancing> *)ash;\n"
   "  raytracing::intersection_query<raytracing::instancing,\n"
   "                                 raytracing::triangle_data> q;\n"
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
   "}\n"
   /* Writes the instance descriptor on the GPU timeline, the way kk's
    * libkk_convert_as_instances does, so that check 3 has a real producer to
    * order the legacy build against rather than a CPU memcpy. */
   "kernel void fill_instance(device uint *dst [[buffer(0)]],\n"
   "                          device const ulong *blas [[buffer(1)]],\n"
   "                          uint tid [[thread_position_in_grid]]) {\n"
   "  float tx[12] = {1,0,0,0, 0,1,0,0, 0,0,1,0};\n"   /* row-major identity */
   "  for (uint i = 0; i < 12; ++i) dst[i] = as_type<uint>(tx[i]);\n"
   "  dst[12] = 1u;          /* options: disable triangle culling */\n"
   "  dst[13] = 0xFFu;       /* mask */\n"
   "  dst[14] = 0u;          /* intersection function table offset */\n"
   "  dst[15] = 0x123456u;   /* userID */\n"
   "  dst[16] = (uint)(blas[0] & 0xFFFFFFFFul);\n"
   "  dst[17] = (uint)(blas[0] >> 32);\n"
   "}\n";

/* MTLIndirectAccelerationStructureInstanceDescriptor, 72 bytes. Same layout kk
 * writes from libkk_convert_as_instances. */
struct indirect_instance {
   float transform[12];
   uint32_t options, mask, ift_offset, user_id;
   uint64_t as_id;
};

struct Out {
   uint32_t type;
   float t;
   uint32_t custom;
   float bx, by;
};

#define NRAYS 2

static id<MTLDevice> dev;
static id<MTLCommandQueue> q3;
static id<MTL4CommandQueue> q4;
static id<MTLResidencySet> rs;
static id<MTLComputePipelineState> pso4_fill;
static id<MTLBuffer> out_buf, ashandle, blashandle;

static int passes, fails;

static void
verdict(int ok, const char *fmt, ...)
{
   va_list ap;
   if (ok)
      ++passes;
   else
      ++fails;
   printf("%s: ", ok ? "PASS" : "FAIL");
   va_start(ap, fmt);
   vprintf(fmt, ap);
   va_end(ap);
   printf("\n");
}

static void
resident(id<MTLAllocation> r)
{
   [rs addAllocation:r];
   [rs commit];
}

/* ---- the three vertices of the one triangle, at z = 1 ---- */
static const float triangle[9] = {0, 0, 1, 1, 0, 1, 0, 1, 1};

/* One heap holding everything, so that every buffer this probe hands to a
 * descriptor is (whole-heap MTLBuffer, nonzero offset) — kk's shape. */
static id<MTLHeap> heap;
static id<MTLBuffer> heap_buf;
static NSUInteger heap_cursor;

static NSUInteger
heap_alloc(NSUInteger size, NSUInteger align)
{
   heap_cursor = (heap_cursor + align - 1) & ~(align - 1);
   NSUInteger at = heap_cursor;
   heap_cursor += size;
   return at;
}

/* Acceleration structures are NOT heap-placed here, because on this device they
 * cannot be. MEASURED: -[MTLHeap newAccelerationStructureWithSize:offset:]
 * returns nil on a placement heap whose storageMode is Shared, and succeeds only
 * on a Private one; kk's heaps are all Shared (KK_MTL_RESOURCE_OPTIONS in
 * kk_private.h), so kk_CreateAccelerationStructureKHR's heap-placement attempt
 * always fails here and always falls through to the standalone
 * -[MTLDevice newAccelerationStructureWithSize:] plus a residency-set entry.
 * That is the shape this probe reproduces. */
static id<MTLAccelerationStructure>
new_as(NSUInteger size)
{
   id<MTLAccelerationStructure> as = [dev newAccelerationStructureWithSize:size];
   if (as)
      resident(as);
   return as;
}

/* ---- submission helpers ---- */

static void
mtl3_submit(void (^body)(id<MTLCommandBuffer> cb))
{
   id<MTLCommandBuffer> cb = [q3 commandBuffer];
   [cb useResidencySet:rs];
   body(cb);
   [cb commit];
   [cb waitUntilCompleted];
   if (cb.error)
      printf("    legacy commit error: %s\n", cb.error.description.UTF8String);
}

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

/* ---- BLAS through the legacy encoder, from heap storage at nonzero offsets ---- */

struct as_placed {
   id<MTLAccelerationStructure> as;
   NSUInteger scratch_off;
   NSUInteger scratch_len;
};

static struct as_placed
blas_legacy_heap(bool indexed)
{
   /* Vertices and (optionally) indices deliberately NOT at offset 0. */
   const NSUInteger vb_off = heap_alloc(sizeof(triangle), 256);
   memcpy((char *)heap_buf.contents + vb_off, triangle, sizeof(triangle));

   NSUInteger ib_off = 0;
   if (indexed) {
      const uint16_t idx[3] = {0, 1, 2};
      ib_off = heap_alloc(sizeof(idx), 256);
      memcpy((char *)heap_buf.contents + ib_off, idx, sizeof(idx));
   }

   MTLAccelerationStructureTriangleGeometryDescriptor *g =
      [MTLAccelerationStructureTriangleGeometryDescriptor descriptor];
   g.vertexBuffer = heap_buf;
   g.vertexBufferOffset = vb_off;
   g.vertexFormat = MTLAttributeFormatFloat3;
   g.vertexStride = 12;
   g.triangleCount = 1;
   g.opaque = YES;
   if (indexed) {
      g.indexBuffer = heap_buf;
      g.indexBufferOffset = ib_off;
      g.indexType = MTLIndexTypeUInt16;
   }

   MTLPrimitiveAccelerationStructureDescriptor *d =
      [MTLPrimitiveAccelerationStructureDescriptor descriptor];
   d.geometryDescriptors = @[ g ];

   MTLAccelerationStructureSizes s =
      [dev accelerationStructureSizesWithDescriptor:d];

   id<MTLAccelerationStructure> as = new_as(s.accelerationStructureSize);
   if (!as) {
      printf("    newAccelerationStructureWithSize: returned nil\n");
      return (struct as_placed){nil, 0, 0};
   }

   const NSUInteger scratch_len = s.buildScratchBufferSize ?: 256;
   const NSUInteger scratch_off = heap_alloc(scratch_len, 256);

   mtl3_submit(^(id<MTLCommandBuffer> cb) {
      id<MTLAccelerationStructureCommandEncoder> enc =
         [cb accelerationStructureCommandEncoder];
      [enc buildAccelerationStructure:as
                          descriptor:d
                       scratchBuffer:heap_buf
                 scratchBufferOffset:scratch_off];
      [enc endEncoding];
   });

   return (struct as_placed){as, scratch_off, scratch_len};
}

/* ---- TLAS through the legacy encoder, three descriptor flavours ---- */

enum tlas_flavour {
   TLAS_INDIRECT_WITH_COUNT_BUFFER,
   TLAS_INDIRECT_NIL_COUNT_BUFFER,
   TLAS_PLAIN_INSTANCE_DESC,
};

/* Writes the instance descriptor on the CPU into heap storage and returns its
 * offset. Check 3 replaces this with a GPU dispatch. */
static NSUInteger
instance_in_heap(id<MTLAccelerationStructure> blas)
{
   const NSUInteger off = heap_alloc(sizeof(struct indirect_instance), 256);
   struct indirect_instance ii = {
      /* row-major 3x4 identity, declared MTLMatrixLayoutRowMajor below —
       * mtl-rt-matrix-layout.m is where that declaration is proved to matter */
      .transform = {1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0},
      .options = MTLAccelerationStructureInstanceOptionDisableTriangleCulling,
      .mask = 0xFF,
      .user_id = 0x123456,
      .as_id = blas.gpuResourceID._impl,
   };
   memcpy((char *)heap_buf.contents + off, &ii, sizeof(ii));
   return off;
}

static MTLAccelerationStructureDescriptor *
tlas_descriptor(enum tlas_flavour flavour, NSUInteger instance_off,
                NSUInteger count_off)
{
   if (flavour == TLAS_PLAIN_INSTANCE_DESC) {
      MTLInstanceAccelerationStructureDescriptor *d =
         [MTLInstanceAccelerationStructureDescriptor descriptor];
      d.instanceDescriptorBuffer = heap_buf;
      d.instanceDescriptorBufferOffset = instance_off;
      d.instanceDescriptorStride = sizeof(struct indirect_instance);
      d.instanceCount = 1;
      d.instanceDescriptorType =
         MTLAccelerationStructureInstanceDescriptorTypeIndirect;
      d.instanceTransformationMatrixLayout = MTLMatrixLayoutRowMajor;
      return d;
   }

   MTLIndirectInstanceAccelerationStructureDescriptor *d =
      [MTLIndirectInstanceAccelerationStructureDescriptor descriptor];
   d.instanceDescriptorBuffer = heap_buf;
   d.instanceDescriptorBufferOffset = instance_off;
   d.instanceDescriptorStride = sizeof(struct indirect_instance);
   d.maxInstanceCount = 1;
   d.instanceDescriptorType =
      MTLAccelerationStructureInstanceDescriptorTypeIndirect;
   d.instanceTransformationMatrixLayout = MTLMatrixLayoutRowMajor;
   if (flavour == TLAS_INDIRECT_WITH_COUNT_BUFFER) {
      d.instanceCountBuffer = heap_buf;
      d.instanceCountBufferOffset = count_off;
   }
   return d;
}

/* MTLIndirectInstanceAccelerationStructureDescriptor is the only one of the
 * three that -accelerationStructureSizesWithDescriptor: is documented to accept
 * for an indirect instance build, so size every flavour with it — which is also
 * what kk does today. */
static MTLAccelerationStructureSizes
tlas_sizes(void)
{
   MTLIndirectInstanceAccelerationStructureDescriptor *sizer =
      [MTLIndirectInstanceAccelerationStructureDescriptor descriptor];
   sizer.instanceDescriptorStride = sizeof(struct indirect_instance);
   sizer.maxInstanceCount = 1;
   sizer.instanceDescriptorType =
      MTLAccelerationStructureInstanceDescriptorTypeIndirect;
   return [dev accelerationStructureSizesWithDescriptor:sizer];
}

static id<MTLAccelerationStructure>
tlas_legacy_heap(id<MTLAccelerationStructure> blas, enum tlas_flavour flavour)
{
   const NSUInteger instance_off = instance_in_heap(blas);
   const NSUInteger count_off = heap_alloc(4, 256);
   *(uint32_t *)((char *)heap_buf.contents + count_off) = 1u;

   MTLAccelerationStructureDescriptor *d =
      tlas_descriptor(flavour, instance_off, count_off);

   MTLAccelerationStructureSizes s = tlas_sizes();
   id<MTLAccelerationStructure> as = new_as(s.accelerationStructureSize);
   const NSUInteger scratch_len = s.buildScratchBufferSize ?: 256;
   const NSUInteger scratch_off = heap_alloc(scratch_len, 256);

   mtl3_submit(^(id<MTLCommandBuffer> cb) {
      id<MTLAccelerationStructureCommandEncoder> enc =
         [cb accelerationStructureCommandEncoder];
      [enc buildAccelerationStructure:as
                          descriptor:d
                       scratchBuffer:heap_buf
                 scratchBufferOffset:scratch_off];
      [enc endEncoding];
   });

   return as;
}

/* ---- trace, from the MTL4 compute encoder (the one kk dispatches on) ---- */

static struct Out
trace_mtl4(id<MTLAccelerationStructure> tlas)
{
   memset(out_buf.contents, 0, NRAYS * sizeof(struct Out));
   *(uint64_t *)ashandle.contents = tlas ? tlas.gpuResourceID._impl : 0;

   static id<MTLComputePipelineState> pso4 = nil;
   if (!pso4) {
      NSError *err = nil;
      id<MTL4Compiler> compiler =
         [dev newCompilerWithDescriptor:[MTL4CompilerDescriptor new] error:&err];
      MTL4LibraryDescriptor *ld = [MTL4LibraryDescriptor new];
      ld.source = [NSString stringWithUTF8String:kernel_src];
      id<MTLLibrary> lib = [compiler newLibraryWithDescriptor:ld error:&err];
      if (!lib) {
         printf("    MTL4 library: %s\n", err.description.UTF8String);
         return (struct Out){0};
      }
      MTL4LibraryFunctionDescriptor *fd = [MTL4LibraryFunctionDescriptor new];
      fd.library = lib;
      fd.name = @"trace";
      MTL4ComputePipelineDescriptor *pd = [MTL4ComputePipelineDescriptor new];
      pd.computeFunctionDescriptor = fd;
      pso4 = [compiler newComputePipelineStateWithDescriptor:pd
                                        compilerTaskOptions:nil
                                                      error:&err];
      if (!pso4) {
         printf("    MTL4 pipeline: %s\n", err.description.UTF8String);
         return (struct Out){0};
      }
   }

   id<MTL4ArgumentTable> at = nil;
   {
      MTL4ArgumentTableDescriptor *atd = [MTL4ArgumentTableDescriptor new];
      atd.maxBufferBindCount = 2;
      NSError *err = nil;
      at = [dev newArgumentTableWithDescriptor:atd error:&err];
   }
   [at setAddress:out_buf.gpuAddress atIndex:0];
   [at setAddress:ashandle.gpuAddress atIndex:1];

   mtl4_submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc setArgumentTable:at];
      [enc setComputePipelineState:pso4];
      [enc dispatchThreads:MTLSizeMake(NRAYS, 1, 1)
        threadsPerThreadgroup:MTLSizeMake(NRAYS, 1, 1)];
   });

   return ((const struct Out *)out_buf.contents)[0];
}

static int
hit_is_right(struct Out o)
{
   return o.type == 1 && o.t > 0.999f && o.t < 1.001f && o.custom == 0x123456 &&
          o.bx > 0.249f && o.bx < 0.251f && o.by > 0.249f && o.by < 0.251f;
}

static void
print_hit(const char *label, struct Out o)
{
   printf("    %s ray0 type=%u t=%.4f custom=0x%x bary=(%.3f,%.3f)\n", label,
          o.type, o.t, o.custom, o.bx, o.by);
}

/* ---- check 3: MTL4 dispatch -> legacy build -> MTL4 trace, event-ordered ---- */

static id<MTLAccelerationStructure>
tlas_event_ordered(id<MTLAccelerationStructure> blas)
{
   /* The instance descriptor is produced by a GPU dispatch on the MTL4 queue,
    * exactly as kk's libkk_convert_as_instances produces it, and the legacy
    * build must not start before that dispatch retires. Zero the destination
    * first so a build that runs early reads zeros and misses, rather than
    * accidentally reading a leftover from an earlier check. */
   const NSUInteger instance_off = heap_alloc(sizeof(struct indirect_instance), 256);
   memset((char *)heap_buf.contents + instance_off, 0,
          sizeof(struct indirect_instance));
   *(uint64_t *)blashandle.contents = blas.gpuResourceID._impl;

   const NSUInteger count_off = heap_alloc(4, 256);
   *(uint32_t *)((char *)heap_buf.contents + count_off) = 1u;

   MTLAccelerationStructureDescriptor *d =
      tlas_descriptor(TLAS_INDIRECT_WITH_COUNT_BUFFER, instance_off, count_off);

   MTLAccelerationStructureSizes s = tlas_sizes();
   id<MTLAccelerationStructure> as = new_as(s.accelerationStructureSize);
   const NSUInteger scratch_off = heap_alloc(s.buildScratchBufferSize ?: 256, 256);

   id<MTLSharedEvent> ev = [dev newSharedEvent];
   ev.signaledValue = 0;

   /* ---- MTL4: produce the instance descriptor. No CPU wait. ---- */
   id<MTL4ArgumentTable> at = nil;
   {
      MTL4ArgumentTableDescriptor *atd = [MTL4ArgumentTableDescriptor new];
      atd.maxBufferBindCount = 2;
      NSError *err = nil;
      at = [dev newArgumentTableWithDescriptor:atd error:&err];
   }
   [at setAddress:heap_buf.gpuAddress + instance_off atIndex:0];
   [at setAddress:blashandle.gpuAddress atIndex:1];

   id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
   id<MTL4CommandBuffer> cb4 = [dev newCommandBuffer];
   [cb4 beginCommandBufferWithAllocator:alloc];
   id<MTL4ComputeCommandEncoder> enc4 = [cb4 computeCommandEncoder];
   [enc4 setArgumentTable:at];
   [enc4 setComputePipelineState:pso4_fill];
   [enc4 dispatchThreads:MTLSizeMake(1, 1, 1)
     threadsPerThreadgroup:MTLSizeMake(1, 1, 1)];
   [enc4 endEncoding];
   [cb4 endCommandBuffer];
   [q4 commit:&cb4 count:1 options:[MTL4CommitOptions new]];
   [q4 signalEvent:ev value:1];

   /* ---- legacy: wait for the dispatch, build, signal ---- */
   id<MTLCommandBuffer> cb3 = [q3 commandBuffer];
   [cb3 useResidencySet:rs];
   [cb3 encodeWaitForEvent:ev value:1];
   id<MTLAccelerationStructureCommandEncoder> enc3 =
      [cb3 accelerationStructureCommandEncoder];
   [enc3 buildAccelerationStructure:as
                        descriptor:d
                     scratchBuffer:heap_buf
               scratchBufferOffset:scratch_off];
   [enc3 endEncoding];
   [cb3 encodeSignalEvent:ev value:2];
   [cb3 commit];

   /* ---- MTL4: wait for the build before anything else on this queue ---- */
   [q4 waitForEvent:ev value:2];

   /* Only now block, and only to read the result back. */
   if (![ev waitUntilSignaledValue:2 timeoutMS:5000]) {
      printf("    TIMEOUT waiting for the event-ordered build (deadlock?)\n");
      return nil;
   }
   if (cb3.error)
      printf("    legacy commit error: %s\n", cb3.error.description.UTF8String);

   return as;
}

int
main(void)
{
   @autoreleasepool {
      dev = MTLCreateSystemDefaultDevice();
      printf("device: %s\n", dev.name.UTF8String);
      for (int f = 9; f >= 1; --f) {
         if ([dev supportsFamily:(MTLGPUFamily)(MTLGPUFamilyApple1 + f - 1)]) {
            printf("apple family: %d\n", f);
            break;
         }
      }
      printf("supportsRaytracing: %s\n", dev.supportsRaytracing ? "YES" : "NO");

      q3 = [dev newCommandQueue];
      q4 = [dev newMTL4CommandQueue];

      MTLResidencySetDescriptor *rsd = [MTLResidencySetDescriptor new];
      NSError *err = nil;
      rs = [dev newResidencySetWithDescriptor:rsd error:&err];
      /* Check 4: the SAME residency set on both queues, kk's arrangement. */
      [q3 addResidencySet:rs];
      [q4 addResidencySet:rs];

      /* One shared heap for geometry, instances, scratch and the structures. */
      MTLHeapDescriptor *hd = [MTLHeapDescriptor new];
      hd.type = MTLHeapTypePlacement;
      hd.storageMode = MTLStorageModeShared;
      hd.size = 4u << 20;
      heap = [dev newHeapWithDescriptor:hd];
      heap_buf = [heap newBufferWithLength:hd.size
                                  options:MTLResourceStorageModeShared
                                   offset:0];
      resident(heap);
      resident(heap_buf);
      /* Deliberately start at a nonzero cursor so nothing lands at offset 0. */
      heap_cursor = 4096;

      out_buf = [dev newBufferWithLength:NRAYS * sizeof(struct Out)
                                options:MTLResourceStorageModeShared];
      ashandle = [dev newBufferWithLength:8 options:MTLResourceStorageModeShared];
      blashandle = [dev newBufferWithLength:8 options:MTLResourceStorageModeShared];
      resident(out_buf);
      resident(ashandle);
      resident(blashandle);

      /* MTL4 pipeline for the instance-filling dispatch used by check 3. */
      {
         id<MTL4Compiler> compiler =
            [dev newCompilerWithDescriptor:[MTL4CompilerDescriptor new] error:&err];
         MTL4LibraryDescriptor *ld = [MTL4LibraryDescriptor new];
         ld.source = [NSString stringWithUTF8String:kernel_src];
         id<MTLLibrary> lib = [compiler newLibraryWithDescriptor:ld error:&err];
         if (!lib) {
            printf("MTL4 library failed: %s\n", err.description.UTF8String);
            return 1;
         }
         MTL4LibraryFunctionDescriptor *fd = [MTL4LibraryFunctionDescriptor new];
         fd.library = lib;
         fd.name = @"fill_instance";
         MTL4ComputePipelineDescriptor *pd = [MTL4ComputePipelineDescriptor new];
         pd.computeFunctionDescriptor = fd;
         pso4_fill = [compiler newComputePipelineStateWithDescriptor:pd
                                                compilerTaskOptions:nil
                                                              error:&err];
         if (!pso4_fill) {
            printf("MTL4 fill pipeline failed: %s\n", err.description.UTF8String);
            return 1;
         }
      }

      printf("\n--- 1. legacy build from heap storage at nonzero offsets ---\n");
      struct as_placed blas = blas_legacy_heap(false);
      if (!blas.as) {
         printf("no BLAS, nothing further is meaningful\n");
         return 1;
      }
      {
         struct Out o = trace_mtl4(tlas_legacy_heap(blas.as,
                                                    TLAS_INDIRECT_WITH_COUNT_BUFFER));
         print_hit("non-indexed ", o);
         verdict(hit_is_right(o),
                 "whole-heap buffer + nonzero vertex/scratch/instance offsets, "
                 "instanceCountBuffer set");
      }
      {
         struct as_placed ib = blas_legacy_heap(true);
         struct Out o = ib.as ? trace_mtl4(tlas_legacy_heap(ib.as,
                                              TLAS_INDIRECT_WITH_COUNT_BUFFER))
                              : (struct Out){0};
         print_hit("indexed     ", o);
         verdict(hit_is_right(o), "same, with a uint16 index buffer at a nonzero "
                                  "indexBufferOffset");
      }

      printf("\n--- 2. does the TLAS descriptor need an instanceCountBuffer? ---\n");
      {
         struct Out o =
            trace_mtl4(tlas_legacy_heap(blas.as, TLAS_INDIRECT_NIL_COUNT_BUFFER));
         print_hit("indirect,nil", o);
         verdict(hit_is_right(o),
                 "MTLIndirectInstanceAccelerationStructureDescriptor with "
                 "instanceCountBuffer = nil");
      }
      {
         struct Out o =
            trace_mtl4(tlas_legacy_heap(blas.as, TLAS_PLAIN_INSTANCE_DESC));
         print_hit("plain,count ", o);
         verdict(hit_is_right(o),
                 "MTLInstanceAccelerationStructureDescriptor + "
                 "instanceDescriptorType = Indirect, plain instanceCount");
      }

      printf("\n--- 3. MTL4 dispatch -> event -> legacy build -> event -> MTL4 "
             "trace ---\n");
      {
         id<MTLAccelerationStructure> t = tlas_event_ordered(blas.as);
         struct Out o = t ? trace_mtl4(t) : (struct Out){0};
         print_hit("ordered     ", o);
         verdict(hit_is_right(o),
                 "MTLSharedEvent orders an MTL4CommandQueue against a legacy "
                 "MTLCommandQueue with no CPU wait between them");
      }

      printf("\n%d passed, %d failed\n", passes, fails);
      return fails ? 1 : 0;
   }
}
