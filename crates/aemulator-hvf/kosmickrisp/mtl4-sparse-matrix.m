/* sparse2.m — placement-sparse residency semantics, measured across submit
 * boundaries.
 *
 * The first cut of this probe wrote and read a sparse buffer inside ONE command
 * encoder and concluded that unbacked tiles keep their writes. That was a
 * measurement artifact: within a single encoder the read can be serviced from
 * cache. Every phase here is its own command-buffer submission, and unbacked
 * tiles are read BEFORE anything is written to them as well as after.
 *
 * Storage-mode acceptance is probed by re-exec (argv), not fork(): forking a
 * process that has already initialised Metal crashes for reasons unrelated to
 * the storage mode, which made the first cut report false refusals.
 *
 * SECTIONS
 *   capabilities        supportsPlacementSparse, tile sizes per page size.
 *   storage modes       which (resource, heap) storage pairs Metal accepts.
 *   buffer              partial backing from TWO heaps, explicit heap tile
 *                       offsets, aliasing, unmap, and reads-return-zero, each
 *                       measured across a submit boundary.
 *   texture             the same for a 2D colour texture, with and without mips.
 *   format sweep        Metal's 2D tile shape against the Vulkan standard sparse
 *                       image block shape for every format family KosmicKrisp
 *                       exposes. This is what justifies the driver claiming
 *                       sparseResidencyStandard2DBlockShape as a blanket
 *                       property rather than per format.
 *   unmap control       the specific operation that does NOT work through the
 *                       driver (see kk_sparse.c), issued here in Metal's exact
 *                       shape to show the hardware honours it. If sparse-smoke.c
 *                       fails its unbind checks and this section passes, the
 *                       driver is what is wrong, not Metal.
 */
#import <Metal/Metal.h>
#import <Metal/MTL4CommandQueue.h>
#import <Metal/MTL4CommandBuffer.h>
#import <Metal/MTL4CommandAllocator.h>
#import <Metal/MTL4ComputeCommandEncoder.h>
#import <Metal/MTL4ArgumentTable.h>
#import <Metal/MTL4Compiler.h>
#import <Metal/MTL4CommitFeedback.h>
#import <Foundation/Foundation.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include <spawn.h>
#include <stdlib.h>

static id<MTLDevice> dev;
static id<MTL4CommandQueue> q4;
static id<MTLResidencySet> rs;
static id<MTL4Compiler> compiler;
static id<MTLLibrary> lib;
static int fails;

/* The page size KosmicKrisp uses: MTL_SPARSE_PAGE_SIZE_DEFAULT. */
#define MTL_SPARSE_PROBE_PAGE MTLSparsePageSize64

#define CHECK(cond, fmt, ...)                                                  \
   do {                                                                        \
      int _c = !!(cond);                                                       \
      if (!_c) fails++;                                                        \
      printf("  [%s] " fmt "\n", _c ? "PASS" : "FAIL", ##__VA_ARGS__);         \
   } while (0)
#define INFO(fmt, ...) printf("  [    ] " fmt "\n", ##__VA_ARGS__)

static const char *kernel_src =
   "#include <metal_stdlib>\n"
   "using namespace metal;\n"
   "struct Args { uint stride; uint count; uint tag; uint pad; };\n"
   "kernel void poke(device uint *buf [[buffer(0)]], constant Args &a [[buffer(2)]],\n"
   "                 uint i [[thread_position_in_grid]]) {\n"
   "  if (i < a.count) buf[i * a.stride] = a.tag + i;\n"
   "}\n"
   "kernel void peek(device const uint *buf [[buffer(0)]], device uint *out [[buffer(1)]],\n"
   "                 constant Args &a [[buffer(2)]], uint i [[thread_position_in_grid]]) {\n"
   "  if (i < a.count) out[i] = buf[i * a.stride];\n"
   "}\n"
   "kernel void poke_tex(texture2d<float, access::write> t [[texture(0)]],\n"
   "                     constant Args &a [[buffer(2)]], uint2 g [[thread_position_in_grid]]) {\n"
   "  t.write(float4(1.0, 0.5, 0.25, 1.0), g * a.stride);\n"
   "}\n"
   "kernel void peek_tex(texture2d<float, access::read> t [[texture(0)]],\n"
   "                     device uint *out [[buffer(1)]], constant Args &a [[buffer(2)]],\n"
   "                     uint2 g [[thread_position_in_grid]]) {\n"
   "  float4 v = t.read(g * a.stride);\n"
   "  out[g.y * a.count + g.x] = uint(v.r * 255.0f + 0.5f);\n"
   "}\n";

struct Args { uint32_t stride, count, tag, pad; };

static void resident(id r) { if (r) { [rs addAllocation:r]; [rs commit]; } }

static id<MTLComputePipelineState>
mkpso(NSString *name)
{
   MTL4ComputePipelineDescriptor *pd = [MTL4ComputePipelineDescriptor new];
   MTL4LibraryFunctionDescriptor *fd = [MTL4LibraryFunctionDescriptor new];
   fd.name = name; fd.library = lib;
   pd.computeFunctionDescriptor = fd;
   NSError *e = nil;
   id<MTLComputePipelineState> p =
      [compiler newComputePipelineStateWithDescriptor:pd compilerTaskOptions:nil error:&e];
   if (!p) printf("pso %s: %s\n", name.UTF8String, e.description.UTF8String);
   return p;
}

/* One submission per call — the point of the rewrite. */
static void
submit(void (^body)(id<MTL4ComputeCommandEncoder> enc))
{
   id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
   id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
   [cb beginCommandBufferWithAllocator:alloc];
   id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
   /* Consume the queue-side mapping updates: the sparse docs make this the
    * caller's responsibility. KK_NO_RS_BARRIER=1 drops it, to measure whether
    * the barrier is load-bearing on this GPU or merely specified. */
   if (!getenv("KK_NO_RS_BARRIER"))
      [enc barrierAfterQueueStages:MTLStageResourceState
                      beforeStages:MTLStageDispatch
                 visibilityOptions:MTL4VisibilityOptionDevice];
   body(enc);
   [enc endEncoding];
   [cb endCommandBuffer];
   MTL4CommitOptions *co = [MTL4CommitOptions new];
   dispatch_semaphore_t sem = dispatch_semaphore_create(0);
   [co addFeedbackHandler:^(id<MTL4CommitFeedback> fb) {
      if (fb.error) printf("  [    ] commit error: %s\n", fb.error.description.UTF8String);
      dispatch_semaphore_signal(sem);
   }];
   [q4 commit:&cb count:1 options:co];
   dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
}

static id<MTLHeap>
placement_heap(uint64_t size, MTLSparsePageSize page, MTLStorageMode mode)
{
   MTLHeapDescriptor *hd = [MTLHeapDescriptor new];
   hd.type = MTLHeapTypePlacement;
   hd.storageMode = mode;
   hd.hazardTrackingMode = MTLHazardTrackingModeUntracked;
   hd.size = size;
   hd.maxCompatiblePlacementSparsePageSize = page;
   return [dev newHeapWithDescriptor:hd];
}

/* ---------------- storage-mode acceptance, one child process each ---------- */
static int
try_modes(MTLStorageMode buf_mode, MTLStorageMode heap_mode)
{
   const MTLSparsePageSize page = MTL_SPARSE_PROBE_PAGE;
   NSUInteger tile = [dev sparseTileSizeInBytesForSparsePageSize:page];
   id<MTLBuffer> sb =
      [dev newBufferWithLength:tile * 2
                      options:(MTLResourceOptions)(buf_mode << MTLResourceStorageModeShift)
      placementSparsePageSize:page];
   if (!sb) return 2;
   id<MTLHeap> hp = placement_heap(tile * 2, page, heap_mode);
   if (!hp) return 3;
   resident(sb); resident(hp);
   MTL4UpdateSparseBufferMappingOperation op = {
      .mode = MTLSparseTextureMappingModeMap, .bufferRange = NSMakeRange(0, 1), .heapOffset = 0};
   [q4 updateBufferMappings:sb heap:hp operations:&op count:1];
   return 0;
}

/* ---------------- buffer ---------------- */
static void
buffer_phase(MTLStorageMode heap_mode)
{
   const MTLSparsePageSize page = MTL_SPARSE_PROBE_PAGE;
   const NSUInteger tile = [dev sparseTileSizeInBytesForSparsePageSize:page];
   const NSUInteger NT = 8;
   printf("\nBUFFER: %lu tiles of %lu B, sparse buffer Private, heaps %s\n",
          (unsigned long)NT, (unsigned long)tile,
          heap_mode == MTLStorageModeShared ? "Shared" : "Private");

   id<MTLBuffer> buf = [dev newBufferWithLength:tile * NT
                                       options:MTLResourceStorageModePrivate
                       placementSparsePageSize:page];
   if (!buf) { CHECK(0, "sparse buffer creation"); return; }
   INFO("gpuAddress 0x%llx sparseBufferTier %ld",
        (unsigned long long)buf.gpuAddress, (long)buf.sparseBufferTier);

   id<MTLHeap> hA = placement_heap(tile * NT, page, heap_mode);
   id<MTLHeap> hB = placement_heap(tile * NT, page, heap_mode);
   if (!hA || !hB) { CHECK(0, "placement heaps"); return; }
   resident(buf); resident(hA); resident(hB);

   id<MTLBuffer> out = [dev newBufferWithLength:NT * 4 options:MTLResourceStorageModeShared];
   id<MTLBuffer> args = [dev newBufferWithLength:sizeof(struct Args)
                                        options:MTLResourceStorageModeShared];
   resident(out); resident(args);
   struct Args *A = args.contents;
   *A = (struct Args){(uint32_t)(tile / 4), (uint32_t)NT, 0xA5A50000u, 0};

   MTL4ArgumentTableDescriptor *atd = [MTL4ArgumentTableDescriptor new];
   atd.maxBufferBindCount = 3;
   NSError *err = nil;
   id<MTL4ArgumentTable> at = [dev newArgumentTableWithDescriptor:atd error:&err];
   [at setAddress:buf.gpuAddress atIndex:0];
   [at setAddress:out.gpuAddress atIndex:1];
   [at setAddress:args.gpuAddress atIndex:2];

   id<MTLComputePipelineState> poke = mkpso(@"poke"), peek = mkpso(@"peek");
   if (!poke || !peek) { CHECK(0, "pipelines"); return; }

   void (^run)(id<MTLComputePipelineState>) = ^(id<MTLComputePipelineState> p) {
      submit(^(id<MTL4ComputeCommandEncoder> enc) {
         [enc setArgumentTable:at];
         [enc setComputePipelineState:p];
         [enc dispatchThreads:MTLSizeMake(NT, 1, 1)
               threadsPerThreadgroup:MTLSizeMake(NT, 1, 1)];
      });
   };
   const uint32_t *o = out.contents;

   /* backed = {0,1} from heap A at heapOffset 0, {4} from heap A at heapOffset 5,
    * {6} from heap B at heapOffset 0. */
   MTL4UpdateSparseBufferMappingOperation op;
   op = (MTL4UpdateSparseBufferMappingOperation){
      .mode = MTLSparseTextureMappingModeMap, .bufferRange = NSMakeRange(0, 2), .heapOffset = 0};
   [q4 updateBufferMappings:buf heap:hA operations:&op count:1];
   op = (MTL4UpdateSparseBufferMappingOperation){
      .mode = MTLSparseTextureMappingModeMap, .bufferRange = NSMakeRange(4, 1), .heapOffset = 5};
   [q4 updateBufferMappings:buf heap:hA operations:&op count:1];
   op = (MTL4UpdateSparseBufferMappingOperation){
      .mode = MTLSparseTextureMappingModeMap, .bufferRange = NSMakeRange(6, 1), .heapOffset = 0};
   [q4 updateBufferMappings:buf heap:hB operations:&op count:1];
   const int backed[8] = {1, 1, 0, 0, 1, 0, 1, 0};

   /* Phase 1: read before ANY write. Freshly mapped tiles must read zero too. */
   memset(out.contents, 0xEE, NT * 4);
   run(peek);
   int zero_ok = 1;
   for (NSUInteger i = 0; i < NT; i++) if (o[i] != 0) zero_ok = 0;
   CHECK(zero_ok, "phase1: all 8 tiles read zero before any write");
   if (!zero_ok)
      for (NSUInteger i = 0; i < NT; i++)
         INFO("   tile %lu = 0x%08x", (unsigned long)i, o[i]);

   /* Phase 2: write every tile, backed or not. Separate submission. */
   run(poke);

   /* Phase 3: read back in yet another submission. */
   memset(out.contents, 0xEE, NT * 4);
   run(peek);
   for (NSUInteger i = 0; i < NT; i++) {
      uint32_t want = backed[i] ? 0xA5A50000u + (uint32_t)i : 0u;
      CHECK(o[i] == want, "tile %lu %-8s read 0x%08x want 0x%08x", (unsigned long)i,
            backed[i] ? "backed" : "UNBACKED", o[i], want);
   }

   /* Aliasing: buffer tile 2 -> the same heap-A tile 0 that buffer tile 0 uses. */
   op = (MTL4UpdateSparseBufferMappingOperation){
      .mode = MTLSparseTextureMappingModeMap, .bufferRange = NSMakeRange(2, 1), .heapOffset = 0};
   [q4 updateBufferMappings:buf heap:hA operations:&op count:1];
   memset(out.contents, 0xEE, NT * 4);
   run(peek);
   CHECK(o[2] == 0xA5A50000u, "alias: tile2 now sees tile0's data 0x%08x (want 0x%08x)",
         o[2], 0xA5A50000u);

   /* Unmap: back to zero, and only for the unmapped range. */
   op = (MTL4UpdateSparseBufferMappingOperation){
      .mode = MTLSparseTextureMappingModeUnmap, .bufferRange = NSMakeRange(0, 2), .heapOffset = 0};
   [q4 updateBufferMappings:buf heap:nil operations:&op count:1];
   memset(out.contents, 0xEE, NT * 4);
   run(peek);
   CHECK(o[0] == 0 && o[1] == 0, "unmap: tiles 0,1 read 0x%08x 0x%08x", o[0], o[1]);
   CHECK(o[4] == 0xA5A50000u + 4, "unmap spared tile 4 (0x%08x)", o[4]);
   CHECK(o[6] == 0xA5A50000u + 6, "unmap spared tile 6, heap B (0x%08x)", o[6]);
}

/* ---------------- texture ---------------- */
static void
texture_phase(MTLStorageMode heap_mode, NSUInteger mips)
{
   const MTLSparsePageSize page = MTL_SPARSE_PROBE_PAGE;
   const NSUInteger tileB = [dev sparseTileSizeInBytesForSparsePageSize:page];
   MTLSize ts = [dev sparseTileSizeWithTextureType:MTLTextureType2D
                                       pixelFormat:MTLPixelFormatRGBA8Unorm
                                       sampleCount:1
                                    sparsePageSize:page];
   const NSUInteger TW = 4, TH = 2;
   printf("\nTEXTURE: RGBA8 2D %lux%lu (%lux%lu tiles of %lux%lu px), %lu mip(s)\n",
          (unsigned long)(ts.width * TW), (unsigned long)(ts.height * TH),
          (unsigned long)TW, (unsigned long)TH,
          (unsigned long)ts.width, (unsigned long)ts.height, (unsigned long)mips);

   MTLTextureDescriptor *td = [MTLTextureDescriptor new];
   td.textureType = MTLTextureType2D;
   td.pixelFormat = MTLPixelFormatRGBA8Unorm;
   td.width = ts.width * TW;
   td.height = ts.height * TH;
   td.mipmapLevelCount = mips;
   td.usage = MTLTextureUsageShaderRead | MTLTextureUsageShaderWrite;
   td.storageMode = MTLStorageModePrivate;
   td.placementSparsePageSize = page;

   id<MTLTexture> tex = [dev newTextureWithDescriptor:td];
   if (!tex) { CHECK(0, "sparse texture creation"); return; }
   INFO("sparseTextureTier %ld firstMipmapInTail %lu tailSizeInBytes %lu",
        (long)tex.sparseTextureTier, (unsigned long)tex.firstMipmapInTail,
        (unsigned long)tex.tailSizeInBytes);

   id<MTLHeap> h = placement_heap(tileB * TW * TH * 2, page, heap_mode);
   if (!h) { CHECK(0, "placement heap"); return; }
   resident(tex); resident(h);

   id<MTLBuffer> out = [dev newBufferWithLength:TW * TH * 4
                                       options:MTLResourceStorageModeShared];
   id<MTLBuffer> args = [dev newBufferWithLength:sizeof(struct Args)
                                        options:MTLResourceStorageModeShared];
   resident(out); resident(args);
   struct Args *A = args.contents;
   *A = (struct Args){(uint32_t)ts.width, (uint32_t)TW, 0, 0};

   MTL4ArgumentTableDescriptor *atd = [MTL4ArgumentTableDescriptor new];
   atd.maxBufferBindCount = 3;
   atd.maxTextureBindCount = 1;
   NSError *err = nil;
   id<MTL4ArgumentTable> at = [dev newArgumentTableWithDescriptor:atd error:&err];
   [at setAddress:out.gpuAddress atIndex:1];
   [at setAddress:args.gpuAddress atIndex:2];
   [at setTexture:tex.gpuResourceID atIndex:0];

   id<MTLComputePipelineState> poke = mkpso(@"poke_tex"), peek = mkpso(@"peek_tex");
   if (!poke || !peek) { CHECK(0, "texture pipelines"); return; }
   void (^run)(id<MTLComputePipelineState>) = ^(id<MTLComputePipelineState> p) {
      submit(^(id<MTL4ComputeCommandEncoder> enc) {
         [enc setArgumentTable:at];
         [enc setComputePipelineState:p];
         [enc dispatchThreads:MTLSizeMake(TW, TH, 1)
               threadsPerThreadgroup:MTLSizeMake(TW, TH, 1)];
      });
   };
   const uint32_t *o = out.contents;

   /* Map the two leftmost tiles of the top tile row. */
   MTL4UpdateSparseTextureMappingOperation op = {
      .mode = MTLSparseTextureMappingModeMap,
      .textureRegion = MTLRegionMake2D(0, 0, 2, 1),
      .textureLevel = 0, .textureSlice = 0, .heapOffset = 0};
   [q4 updateTextureMappings:tex heap:h operations:&op count:1];

   memset(out.contents, 0xEE, TW * TH * 4);
   run(peek);
   int zero_ok = 1;
   for (NSUInteger i = 0; i < TW * TH; i++) if (o[i] != 0) zero_ok = 0;
   CHECK(zero_ok, "phase1: all tiles read zero before any write");

   run(poke);
   memset(out.contents, 0xEE, TW * TH * 4);
   run(peek);
   for (NSUInteger y = 0; y < TH; y++)
      for (NSUInteger x = 0; x < TW; x++) {
         int backed = (y == 0 && x < 2);
         CHECK(o[y * TW + x] == (backed ? 255u : 0u), "tile (%lu,%lu) %-8s read %u want %u",
               (unsigned long)x, (unsigned long)y, backed ? "backed" : "UNBACKED",
               o[y * TW + x], backed ? 255u : 0u);
      }

   op.mode = MTLSparseTextureMappingModeUnmap;
   [q4 updateTextureMappings:tex heap:nil operations:&op count:1];
   memset(out.contents, 0xEE, TW * TH * 4);
   run(peek);
   CHECK(o[0] == 0 && o[1] == 0, "unmap: tiles (0,0),(1,0) read %u %u", o[0], o[1]);
}


/* ---------------- format sweep: Metal tile vs the Vulkan standard ---------- */
struct sweep_fmt {
   const char *name;
   MTLPixelFormat fmt;
   uint32_t elem_B, bw, bh; /* bytes per element; element footprint in texels */
};

static const struct sweep_fmt sweep_fmts[] = {
   {"R8Unorm",        MTLPixelFormatR8Unorm,         1, 1, 1},
   {"A8Unorm",        MTLPixelFormatA8Unorm,         1, 1, 1},
   {"R16Float",       MTLPixelFormatR16Float,        2, 1, 1},
   {"RG8Unorm",       MTLPixelFormatRG8Unorm,        2, 1, 1},
   {"B5G6R5Unorm",    MTLPixelFormatB5G6R5Unorm,     2, 1, 1},
   {"RGBA8Unorm",     MTLPixelFormatRGBA8Unorm,      4, 1, 1},
   {"BGRA8Unorm",     MTLPixelFormatBGRA8Unorm,      4, 1, 1},
   {"R32Float",       MTLPixelFormatR32Float,        4, 1, 1},
   {"RGB10A2Unorm",   MTLPixelFormatRGB10A2Unorm,    4, 1, 1},
   {"RG11B10Float",   MTLPixelFormatRG11B10Float,    4, 1, 1},
   {"Depth32Float",   MTLPixelFormatDepth32Float,    4, 1, 1},
   {"Depth16Unorm",   MTLPixelFormatDepth16Unorm,    2, 1, 1},
   {"Stencil8",       MTLPixelFormatStencil8,        1, 1, 1},
   {"RGBA16Float",    MTLPixelFormatRGBA16Float,     8, 1, 1},
   {"RG32Float",      MTLPixelFormatRG32Float,       8, 1, 1},
   {"RGBA32Float",    MTLPixelFormatRGBA32Float,    16, 1, 1},
   {"BC1_RGBA",       MTLPixelFormatBC1_RGBA,        8, 4, 4},
   {"BC3_RGBA",       MTLPixelFormatBC3_RGBA,       16, 4, 4},
   {"BC4_RUnorm",     MTLPixelFormatBC4_RUnorm,      8, 4, 4},
   {"BC5_RGUnorm",    MTLPixelFormatBC5_RGUnorm,    16, 4, 4},
   {"BC6H_RGBFloat",  MTLPixelFormatBC6H_RGBFloat,  16, 4, 4},
   {"BC7_RGBAUnorm",  MTLPixelFormatBC7_RGBAUnorm,  16, 4, 4},
   {"ETC2_RGB8",      MTLPixelFormatETC2_RGB8,       8, 4, 4},
   {"EAC_RGBA8",      MTLPixelFormatEAC_RGBA8,      16, 4, 4},
   {"ASTC_4x4_LDR",   MTLPixelFormatASTC_4x4_LDR,   16, 4, 4},
   {"ASTC_8x8_LDR",   MTLPixelFormatASTC_8x8_LDR,   16, 8, 8},
   {"ASTC_12x12_LDR", MTLPixelFormatASTC_12x12_LDR, 16, 12, 12},
};

/* The spec's table is 256x256 / 256x128 / 128x128 / 128x64 / 64x64 for
 * 1/2/4/8/16-byte elements: each doubling halves the height, then the width. */
static void
vk_standard_shape(uint32_t elem_B, uint32_t *w, uint32_t *h)
{
   uint32_t k = 31u - __builtin_clz(elem_B < 1 ? 1 : elem_B);
   *w = 256u >> (k / 2u);
   *h = 256u >> ((k + 1u) / 2u);
}

static void
format_sweep(void)
{
   const MTLSparsePageSize page = MTL_SPARSE_PROBE_PAGE;
   printf("\nFORMAT SWEEP at the driver's page size (%lu B/tile)\n",
          (unsigned long)[dev sparseTileSizeInBytesForSparsePageSize:page]);
   int diff = 0;
   for (unsigned i = 0; i < sizeof sweep_fmts / sizeof sweep_fmts[0]; i++) {
      MTLSize t = [dev sparseTileSizeWithTextureType:MTLTextureType2D
                                        pixelFormat:sweep_fmts[i].fmt
                                        sampleCount:1
                                     sparsePageSize:page];
      uint32_t w_el, h_el;
      vk_standard_shape(sweep_fmts[i].elem_B, &w_el, &h_el);
      uint32_t ww = w_el * sweep_fmts[i].bw, hh = h_el * sweep_fmts[i].bh;
      int match = t.width == ww && t.height == hh && t.depth == 1;
      if (!match) diff++;
      printf("  [%s] %-16s metal %4lux%-4lu vulkan %4ux%-4u\n", match ? "ok  " : "DIFF",
             sweep_fmts[i].name, (unsigned long)t.width, (unsigned long)t.height, ww, hh);
   }
   CHECK(diff == 0,
         "every format's 2D tile is the Vulkan standard block shape "
         "(%d deviation(s)) -> sparseResidencyStandard2DBlockShape is claimable",
         diff);
}

/* ---------------- unmap control ------------------------------------------- */
/* Reproduces KosmicKrisp's surroundings as closely as a probe can: a placement
 * heap that also carries a full-size host window at offset 0 (kk_alloc_bo makes
 * one for every VkDeviceMemory), a Private placement sparse buffer over it, a
 * shader write through a raw device address (kk's vkCmdFillBuffer is a compute
 * meta pass that stores through one), then Unmap, then a read. If this passes
 * while sparse-smoke.c's unbind checks fail, Metal is not the problem. */
static void
unmap_control(void)
{
   const MTLSparsePageSize page = MTL_SPARSE_PROBE_PAGE;
   const NSUInteger tile = [dev sparseTileSizeInBytesForSparsePageSize:page];
   printf("\nUNMAP CONTROL (kk's heap shape, shader write, then Unmap)\n");

   MTLHeapDescriptor *hd = [MTLHeapDescriptor new];
   hd.type = MTLHeapTypePlacement;
   hd.storageMode = MTLStorageModeShared;
   hd.hazardTrackingMode = MTLHazardTrackingModeUntracked;
   hd.size = tile * 2;
   hd.sparsePageSize = MTLSparsePageSize16;   /* kk sets this too */
   hd.maxCompatiblePlacementSparsePageSize = page;
   id<MTLHeap> hp = [dev newHeapWithDescriptor:hd];
   id<MTLBuffer> window = [hp newBufferWithLength:tile * 2
                                         options:MTLResourceStorageModeShared
                                          offset:0];
   id<MTLBuffer> buf = [dev newBufferWithLength:tile * 2
                                       options:MTLResourceStorageModePrivate
                       placementSparsePageSize:page];
   id<MTLBuffer> out = [dev newBufferWithLength:64 options:MTLResourceStorageModeShared];
   id<MTLBuffer> args = [dev newBufferWithLength:sizeof(struct Args)
                                        options:MTLResourceStorageModeShared];
   if (!hp || !window || !buf) { CHECK(0, "unmap control setup"); return; }
   resident(hp); resident(window); resident(buf); resident(out); resident(args);

   struct Args *A = args.contents;
   *A = (struct Args){(uint32_t)(tile / 4), 2u, 0x11112222u, 0};

   NSError *err = nil;
   MTL4ArgumentTableDescriptor *atd = [MTL4ArgumentTableDescriptor new];
   atd.maxBufferBindCount = 3;
   id<MTL4ArgumentTable> at = [dev newArgumentTableWithDescriptor:atd error:&err];
   [at setAddress:buf.gpuAddress atIndex:0];
   [at setAddress:out.gpuAddress atIndex:1];
   [at setAddress:args.gpuAddress atIndex:2];

   id<MTLComputePipelineState> poke = mkpso(@"poke"), peek = mkpso(@"peek");
   if (!poke || !peek) { CHECK(0, "unmap control pipelines"); return; }

   MTL4UpdateSparseBufferMappingOperation op = {
      .mode = MTLSparseTextureMappingModeMap,
      .bufferRange = NSMakeRange(0, 1), .heapOffset = 0};
   [q4 updateBufferMappings:buf heap:hp operations:&op count:1];

   submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc setArgumentTable:at];
      [enc setComputePipelineState:poke];
      [enc dispatchThreads:MTLSizeMake(1,1,1) threadsPerThreadgroup:MTLSizeMake(1,1,1)];
   });
   memset(out.contents, 0xEE, 64);
   submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc setArgumentTable:at];
      [enc setComputePipelineState:peek];
      [enc dispatchThreads:MTLSizeMake(2,1,1) threadsPerThreadgroup:MTLSizeMake(2,1,1)];
   });
   CHECK(((uint32_t *)out.contents)[0] == 0x11112222u,
         "while mapped, the shader write is visible (0x%08x)",
         ((uint32_t *)out.contents)[0]);

   op.mode = MTLSparseTextureMappingModeUnmap;
   [q4 updateBufferMappings:buf heap:nil operations:&op count:1];
   memset(out.contents, 0xEE, 64);
   submit(^(id<MTL4ComputeCommandEncoder> enc) {
      [enc setArgumentTable:at];
      [enc setComputePipelineState:peek];
      [enc dispatchThreads:MTLSizeMake(2,1,1) threadsPerThreadgroup:MTLSizeMake(2,1,1)];
   });
   CHECK(((uint32_t *)out.contents)[0] == 0u,
         "after Unmap, Metal reads zero (0x%08x) — the driver does not, see "
         "kk_sparse.c", ((uint32_t *)out.contents)[0]);
}

/* ---------------- main ---------------- */
int
main(int argc, char **argv)
{
   @autoreleasepool {
      dev = MTLCreateSystemDefaultDevice();
      q4 = [dev newMTL4CommandQueue];
      NSError *err = nil;
      rs = [dev newResidencySetWithDescriptor:[MTLResidencySetDescriptor new] error:&err];
      [q4 addResidencySet:rs];
      compiler = [dev newCompilerWithDescriptor:[MTL4CompilerDescriptor new] error:&err];
      MTL4LibraryDescriptor *ld = [MTL4LibraryDescriptor new];
      ld.source = [NSString stringWithUTF8String:kernel_src];
      lib = [compiler newLibraryWithDescriptor:ld error:&err];
      if (!lib) { printf("library: %s\n", err.description.UTF8String); return 1; }

      /* Child mode: "modes <bufMode> <heapMode>". */
      if (argc == 4 && !strcmp(argv[1], "modes"))
         return try_modes((MTLStorageMode)atoi(argv[2]), (MTLStorageMode)atoi(argv[3]));

      printf("device: %s (Apple8 %d Apple9 %d Metal4 %d)\n", dev.name.UTF8String,
             [dev supportsFamily:MTLGPUFamilyApple8], [dev supportsFamily:MTLGPUFamilyApple9],
             [dev supportsFamily:MTLGPUFamilyMetal4]);
      if ([dev respondsToSelector:@selector(supportsPlacementSparse)])
         INFO("supportsPlacementSparse = %d", (int)dev.supportsPlacementSparse);

      printf("\nstorage-mode acceptance (each in its own process)\n");
      MTLStorageMode modes[] = {MTLStorageModeShared, MTLStorageModePrivate};
      const char *nm[] = {"Shared", "?", "Private"};
      for (int b = 0; b < 2; b++)
         for (int h = 0; h < 2; h++) {
            char a2[8], a3[8];
            snprintf(a2, sizeof a2, "%d", (int)modes[b]);
            snprintf(a3, sizeof a3, "%d", (int)modes[h]);
            pid_t pid; int st = 0;
            char *av[] = {argv[0], (char *)"modes", a2, a3, NULL};
            posix_spawn(&pid, argv[0], NULL, NULL, av, NULL);
            waitpid(pid, &st, 0);
            int ok = WIFEXITED(st) && WEXITSTATUS(st) == 0;
            printf("  [%s] sparse buffer %-7s + heap %-7s -> %s\n", ok ? "ok  " : "----",
                   nm[modes[b]], nm[modes[h]],
                   ok ? "accepted" : WIFEXITED(st) ? "refused (nil)" : "refused (assertion)");
         }

      buffer_phase(MTLStorageModePrivate);
      buffer_phase(MTLStorageModeShared);
      texture_phase(MTLStorageModePrivate, 1);
      texture_phase(MTLStorageModePrivate, 4);
      format_sweep();
      unmap_control();

      printf("\n%d FAIL(s)\n", fails);
      return fails;
   }
}
