/* mtl-heap-addr-probe.m — the Metal facts the device-address -> MTLBuffer
 * resolution rests on, measured in pure Metal with no Vulkan and no KosmicKrisp.
 *
 * kk gives every VkDeviceMemory an MTLHeapTypePlacement heap plus one whole-heap
 * MTLBuffer at offset 0 (kk_bo::map / ::gpu / ::size_B, see kk_bo.c), and then
 * suballocates each VkBuffer out of that heap with
 * -[MTLHeap newBufferWithLength:options:offset:] at VkBindBufferMemoryInfo::
 * memoryOffset. kk_device_address.c resolves an arbitrary VkDeviceAddress by
 * finding the mapping whose address range contains it and returning
 * (that mapping's MTLBuffer, addr - mapping base).
 *
 * That is only correct if a heap-placed buffer's gpuAddress is exactly
 * wholeHeapBuffer.gpuAddress + offset. "Placement" says the resource occupies
 * heap bytes [offset, offset+size), which strongly implies it, but implying is
 * not measuring. This measures it, and it also measures that the address ranges
 * of coexisting allocations — two heaps, a device-level placement-sparse buffer,
 * a no-copy host import — never overlap, which is why they can all share one
 * flat non-overlapping interval map.
 *
 * NOTE the corollary this also demonstrates: Metal *recycles* virtual addresses
 * as soon as a resource is released, so a stale map entry does not merely point
 * at dead memory, it can alias a live unrelated allocation. Removal on free is
 * load-bearing, not hygiene. (Drop the keep-alive array below and heap 2 lands
 * on exactly the address the released sparse buffer had.)
 *
 * Every line it prints is a read-back number. Exit 0 iff every check holds.
 *
 *   clang -fobjc-arc -O1 -o probe mtl-heap-addr-probe.m -framework Metal -framework Foundation
 */

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>

/* Exactly KK_MTL_RESOURCE_OPTIONS from src/kosmickrisp/vulkan/kk_private.h. */
static const MTLResourceOptions KK_OPTS =
   MTLResourceStorageModeShared | MTLResourceCPUCacheModeDefaultCache |
   MTLResourceHazardTrackingModeUntracked;

static unsigned pass_count, fail_count;

static void
check(bool ok, const char *fmt, ...)
{
   va_list ap;
   va_start(ap, fmt);
   printf(ok ? "  ok   " : "  FAIL ");
   vprintf(fmt, ap);
   printf("\n");
   va_end(ap);
   if (ok)
      ++pass_count;
   else
      ++fail_count;
}

/* [base, base+len) pairs of every resource we hold, so the disjointness claims
 * are about resources that are alive at the same time. */
#define MAX_RANGES 32
static struct {
   const char *name;
   uint64_t base, len;
} ranges[MAX_RANGES];
static unsigned range_count;

static bool
range_is_disjoint(const char *name, uint64_t base, uint64_t len)
{
   bool ok = true;
   for (unsigned i = 0; i < range_count; ++i) {
      if (base < ranges[i].base + ranges[i].len && ranges[i].base < base + len) {
         printf("       overlaps %s [0x%" PRIx64 ", 0x%" PRIx64 ")\n",
                ranges[i].name, ranges[i].base, ranges[i].base + ranges[i].len);
         ok = false;
      }
   }
   if (range_count < MAX_RANGES)
      ranges[range_count++] = (typeof(ranges[0])){name, base, len};
   return ok;
}

int
main(void)
{
   @autoreleasepool {
      /* Nothing here is ever released before the process exits: see the note at
       * the top about address recycling. */
      NSMutableArray *keep = [NSMutableArray array];

      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      if (dev == nil) {
         fprintf(stderr, "no Metal device\n");
         return 77;
      }
      printf("device: %s\n", dev.name.UTF8String);

      /* --- the heap, exactly as kk_alloc_bo() builds it ------------------- */
      const uint64_t heap_size = 4ull << 20;
      MTLHeapDescriptor *hd = [MTLHeapDescriptor new];
      hd.type = MTLHeapTypePlacement;
      hd.resourceOptions = KK_OPTS;
      hd.size = heap_size;
      hd.sparsePageSize = MTLSparsePageSize16;
      if ([hd respondsToSelector:@selector
              (setMaxCompatiblePlacementSparsePageSize:)])
         hd.maxCompatiblePlacementSparsePageSize = MTLSparsePageSize16;

      id<MTLHeap> heap = [dev newHeapWithDescriptor:hd];
      if (heap == nil) {
         fprintf(stderr, "newHeapWithDescriptor: failed\n");
         return 1;
      }
      [keep addObject:heap];

      id<MTLBuffer> whole = [heap newBufferWithLength:heap.size
                                             options:KK_OPTS
                                              offset:0];
      if (whole == nil) {
         fprintf(stderr, "whole-heap newBufferWithLength:offset:0 failed\n");
         return 1;
      }
      [keep addObject:whole];
      const uint64_t base = whole.gpuAddress;
      printf("heap.size          = %" PRIu64 " (asked %" PRIu64 ")\n",
             (uint64_t)heap.size, heap_size);
      printf("whole.length       = %" PRIu64 "\n", (uint64_t)whole.length);
      printf("whole.gpuAddress   = 0x%" PRIx64 "\n", base);

      check(whole.length >= heap_size,
            "the whole-heap buffer covers the allocation (%" PRIu64
            " >= %" PRIu64 ")",
            (uint64_t)whole.length, heap_size);
      range_is_disjoint("heap 1", base, whole.length);

      /* --- the invariant: placed buffer address == base + offset ---------- */
      const uint64_t offsets[] = {0, 4096, 65536, 262144, 1048576, 3145728};
      for (unsigned i = 0; i < sizeof(offsets) / sizeof(offsets[0]); ++i) {
         const uint64_t off = offsets[i];
         id<MTLBuffer> sub = [heap newBufferWithLength:65536
                                              options:KK_OPTS
                                               offset:off];
         if (sub == nil) {
            check(false,
                  "newBufferWithLength:65536 offset:%" PRIu64 " returned nil",
                  off);
            continue;
         }
         [keep addObject:sub];
         const uint64_t got = sub.gpuAddress;
         check(got == base + off,
               "offset %-8" PRIu64 " gpuAddress 0x%" PRIx64
               " == base+offset 0x%" PRIx64 " (delta %+" PRId64 ")",
               off, got, base + off, (int64_t)(got - base - off));
      }

      /* An acceleration structure placed in the same heap must not disturb the
       * buffer addressing around it: the legacy AS builder writes into one while
       * reading geometry resolved out of the very same heap. */
      if ([dev respondsToSelector:@selector
               (heapAccelerationStructureSizeAndAlignWithSize:)]) {
         const uint64_t as_size = 65536;
         MTLSizeAndAlign sa =
            [dev heapAccelerationStructureSizeAndAlignWithSize:as_size];
         const uint64_t as_off = 2097152;
         id<MTLAccelerationStructure> as =
            [heap newAccelerationStructureWithSize:as_size offset:as_off];
         if (as != nil)
            [keep addObject:as];
         printf("heapASSizeAndAlign(%" PRIu64 ") = size %" PRIu64
                " align %" PRIu64 "; newAccelerationStructureWithSize:%" PRIu64
                " offset:%" PRIu64 " -> %s\n",
                as_size, (uint64_t)sa.size, (uint64_t)sa.align, as_size, as_off,
                as != nil ? "non-nil" : "NIL");
         if (as == nil) {
            /* Measured, and not a check because it is not about addressing:
             * MTLHeapTypePlacement + Shared storage refuses
             * newAccelerationStructureWithSize:offset: on this M2, at every
             * offset including 0, with and without
             * maxCompatiblePlacementSparsePageSize. Switching only the heap's
             * storageMode to Private makes the same call succeed. kk's heaps are
             * Shared (KK_MTL_RESOURCE_OPTIONS), so
             * mtl_heap_new_acceleration_structure_with_size() always returns nil
             * and kk_CreateAccelerationStructureKHR always takes its standalone
             * fallback — the Metal structure never actually lives inside the
             * application's VkBuffer range here. */
            printf("       (a placed acceleration structure needs a PRIVATE "
                   "heap; kk's heaps are Shared, so kk always falls back to a "
                   "standalone MTLAccelerationStructure)\n");
         }

         const uint64_t after_off = as_off + sa.size;
         id<MTLBuffer> after = [heap newBufferWithLength:4096
                                                options:KK_OPTS
                                                 offset:after_off];
         if (after != nil)
            [keep addObject:after];
         check(after != nil && after.gpuAddress == base + after_off,
               "a buffer placed just past a heap acceleration structure still "
               "addresses as base+offset (want 0x%" PRIx64 ", got 0x%" PRIx64
               ")",
               base + after_off, after != nil ? after.gpuAddress : 0ull);
      }

      /* --- placement-sparse buffers get their own VA ---------------------- */
      if ([dev respondsToSelector:@selector
               (newBufferWithLength:options:placementSparsePageSize:)]) {
         id<MTLBuffer> sparse =
            [dev newBufferWithLength:1048576
                            options:MTLResourceStorageModePrivate
            placementSparsePageSize:MTLSparsePageSize16];
         if (sparse == nil) {
            check(false, "placement-sparse newBufferWithLength: returned nil");
         } else {
            [keep addObject:sparse];
            printf("sparse.gpuAddress  = 0x%" PRIx64 " length %" PRIu64 "\n",
                   sparse.gpuAddress, (uint64_t)sparse.length);
            check(sparse.gpuAddress != 0 &&
                     range_is_disjoint("sparse buffer", sparse.gpuAddress,
                                       sparse.length),
                  "a live placement-sparse buffer's VA is disjoint from every "
                  "heap");
         }
      }

      /* --- a second heap must not overlap the first ----------------------- */
      id<MTLHeap> heap2 = [dev newHeapWithDescriptor:hd];
      id<MTLBuffer> whole2 = [heap2 newBufferWithLength:heap2.size
                                               options:KK_OPTS
                                                offset:0];
      if (whole2 != nil) {
         [keep addObject:heap2];
         [keep addObject:whole2];
         printf("heap2.gpuAddress   = 0x%" PRIx64 "\n", whole2.gpuAddress);
         check(range_is_disjoint("heap 2", whole2.gpuAddress, whole2.length),
               "two live heaps occupy disjoint virtual ranges");
      }

      /* --- newBufferWithBytesNoCopy, the host-import path ----------------- */
      void *host = NULL;
      const size_t host_size = 65536;
      if (posix_memalign(&host, 16384, host_size) == 0) {
         id<MTLBuffer> imported = [dev newBufferWithBytesNoCopy:host
                                                        length:host_size
                                                       options:KK_OPTS
                                                   deallocator:nil];
         if (imported != nil) {
            [keep addObject:imported];
            printf("import.gpuAddress  = 0x%" PRIx64 " length %" PRIu64 "\n",
                   imported.gpuAddress, (uint64_t)imported.length);
            check(imported.gpuAddress != 0 &&
                     range_is_disjoint("host import", imported.gpuAddress,
                                       imported.length),
                  "a live no-copy host import has its own nonzero VA, disjoint "
                  "from every heap");
         }
      }

      /* --- what a publish costs, relative to what it sits next to ---------
       * kk_addr_map_publish() runs immediately after this pair in
       * kk_alloc_bo(). addr-map-test prints the publish side; this is the
       * denominator. Informational, not a check — it is a timing. */
      {
         const unsigned n = 64;
         struct timespec t0, t1;
         clock_gettime(CLOCK_MONOTONIC, &t0);
         for (unsigned i = 0; i < n; ++i) {
            id<MTLHeap> h = [dev newHeapWithDescriptor:hd];
            id<MTLBuffer> b = [h newBufferWithLength:h.size
                                            options:KK_OPTS
                                             offset:0];
            (void)b.gpuAddress;
            [keep addObject:h];
            [keep addObject:b];
         }
         clock_gettime(CLOCK_MONOTONIC, &t1);
         const double ms = (t1.tv_sec - t0.tv_sec) * 1e3 +
                           (t1.tv_nsec - t0.tv_nsec) / 1e6;
         printf("\nnewHeapWithDescriptor: + whole-heap newBufferWithLength: "
                "(%" PRIu64 " KiB): %.4f ms each over %u\n",
                heap_size >> 10, ms / n, n);
      }

      printf("\n%u/%u checks passed\n", pass_count, pass_count + fail_count);
      (void)keep;
      return fail_count == 0 ? 0 : 1;
   }
}
