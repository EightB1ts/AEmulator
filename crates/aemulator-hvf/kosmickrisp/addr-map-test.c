/* addr-map-test.c — unit test for kk_addr_map, the VkDeviceAddress -> MTLBuffer
 * interval map in src/kosmickrisp/vulkan/kk_device_address.c.
 *
 * It #includes that .c file so the code under test is the real driver source,
 * not a copy. The only thing shimmed is simple_mtx (pthreads instead of mesa's
 * futex mutex), which is why this builds with one clang invocation and needs
 * neither the mesa build nor Metal nor a GPU. kk_device_address.c is written to
 * depend on nothing else for exactly this reason; if that stops being true, this
 * test stops compiling, which is the intended alarm.
 *
 * The interesting part is not the hand-written cases but MODEL CHECKING: a few
 * hundred thousand random publish/withdraw/lookup operations, with every lookup
 * cross-checked against a dumb linear scan over an independent reference array,
 * plus a check after every mutation that the array really is sorted, disjoint
 * and the same size as the model. A binary search over an array kept sorted by
 * memmove is exactly the kind of code where an off-by-one hides from
 * hand-written cases.
 *
 * The concurrency leg exists because vkAllocateMemory, vkFreeMemory and
 * vkCmdBuildAccelerationStructuresKHR are only externally synchronised
 * per-object, so publish, withdraw and lookup genuinely do run at the same time
 * in a real application. Run it under -fsanitize=thread (run-addr-map-test.sh
 * does) for that leg to mean anything.
 *
 * Built and run by crates/aemulator-hvf/kosmickrisp/run-addr-map-test.sh.
 */

#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* ---- the simple_mtx shim ------------------------------------------------- */
/* Claim util/simple_mtx.h's include guard so the real header no-ops, then
 * provide the four entry points kk_device_address.c uses. mesa's version is a
 * futex mutex with an inlined fast path; nothing under test depends on that, only
 * on mutual exclusion. */
#define _SIMPLE_MTX_H
#define mtx_plain 0
typedef pthread_mutex_t simple_mtx_t;
static inline void
simple_mtx_init(simple_mtx_t *m, int type)
{
   (void)type;
   pthread_mutex_init(m, NULL);
}
static inline void
simple_mtx_destroy(simple_mtx_t *m)
{
   pthread_mutex_destroy(m);
}
static inline void
simple_mtx_lock(simple_mtx_t *m)
{
   pthread_mutex_lock(m);
}
static inline void
simple_mtx_unlock(simple_mtx_t *m)
{
   pthread_mutex_unlock(m);
}

#include "kk_device_address.c"

/* ---- test harness ------------------------------------------------------- */

static unsigned pass_count, fail_count;

#define CHECK(cond, ...)                                                       \
   do {                                                                        \
      if (cond) {                                                              \
         ++pass_count;                                                         \
      } else {                                                                 \
         ++fail_count;                                                         \
         printf("  FAIL %s:%d ", __FILE__, __LINE__);                          \
         printf(__VA_ARGS__);                                                  \
         printf("\n");                                                         \
      }                                                                        \
   } while (0)

static void
group(const char *name)
{
   printf("== %s\n", name);
}

/* Distinct non-NULL buffer handles. The map never dereferences one. */
static char fake_buffers[64];
#define BUF(i) ((mtl_buffer *)&fake_buffers[(i)])

/* ---- 1. hand-written edge cases ----------------------------------------- */

static void
test_edges(void)
{
   struct kk_addr_map map;
   struct kk_addr_ref ref;

   group("edge cases");
   kk_addr_map_init(&map);

   CHECK(!kk_addr_map_lookup(&map, 0x1000, 0, &ref),
         "lookup in an empty map must fail");
   CHECK(ref.buffer == NULL && ref.offset_B == 0 && ref.size_B == 0,
         "a failed lookup must zero its output");

   /* Refusals. */
   CHECK(!kk_addr_map_publish(&map, 0, 0x1000, BUF(0), "zero base"),
         "address 0 is Vulkan's null address and must not be publishable");
   CHECK(!kk_addr_map_publish(&map, 0x1000, 0, BUF(0), "zero size"),
         "a zero-length range can never contain an address");
   CHECK(!kk_addr_map_publish(&map, 0x1000, 0x1000, NULL, "null buffer"),
         "a NULL buffer would resolve to nothing usable");
   CHECK(!kk_addr_map_publish(&map, UINT64_MAX - 4, 16, BUF(0), "wrapping"),
         "a range that wraps past 2^64 must be refused");
   CHECK(map.count == 0, "no refused publish may leave an entry (count %u)",
         map.count);

   /* One mapping, every boundary. */
   CHECK(kk_addr_map_publish(&map, 0x2000, 0x1000, BUF(1), "one"),
         "a well-formed publish must succeed");

   CHECK(!kk_addr_map_lookup(&map, 0x1fff, 0, &ref),
         "one byte below the base is outside");
   CHECK(kk_addr_map_lookup(&map, 0x2000, 0, &ref) && ref.buffer == BUF(1) &&
            ref.offset_B == 0 && ref.size_B == 0x1000,
         "the base resolves at offset 0 with the whole length remaining");
   CHECK(kk_addr_map_lookup(&map, 0x2abc, 0, &ref) && ref.buffer == BUF(1) &&
            ref.offset_B == 0xabc && ref.size_B == 0x1000 - 0xabc,
         "a mid-range address resolves to base+offset");
   CHECK(kk_addr_map_lookup(&map, 0x2fff, 0, &ref) && ref.offset_B == 0xfff &&
            ref.size_B == 1,
         "the last byte resolves with one byte remaining");
   CHECK(!kk_addr_map_lookup(&map, 0x3000, 0, &ref),
         "one past the end is outside — ranges are half-open");

   /* Size constraints. */
   CHECK(kk_addr_map_lookup(&map, 0x2000, 0x1000, &ref),
         "a request for exactly the whole mapping fits");
   CHECK(!kk_addr_map_lookup(&map, 0x2000, 0x1001, &ref),
         "one byte more than the mapping must fail, not clamp");
   CHECK(kk_addr_map_lookup(&map, 0x2f00, 0x100, &ref),
         "a request ending exactly at the end fits");
   CHECK(!kk_addr_map_lookup(&map, 0x2f00, 0x101, &ref),
         "a request running one byte past the end must fail");

   /* The mtl_buffer_range flavour, including MTL_BUFFER_RANGE_WHOLE. */
   struct mtl_buffer_range whole = {.address = 0x2800,
                                    .length_B = MTL_BUFFER_RANGE_WHOLE};
   CHECK(kk_addr_map_lookup_range(&map, whole, &ref) && ref.offset_B == 0x800 &&
            ref.size_B == 0x800,
         "MTL_BUFFER_RANGE_WHOLE resolves and reports the bytes remaining");
   struct mtl_buffer_range too_big = {.address = 0x2800, .length_B = 0x801};
   CHECK(!kk_addr_map_lookup_range(&map, too_big, &ref),
         "an explicit length past the end fails through the range flavour too");

   /* Overlaps, all four shapes, against [0x2000, 0x3000). The assert() inside
    * kk_addr_map_publish() is compiled out here (-DNDEBUG, as in the release
    * driver) so that the refusal itself is what gets tested. */
   CHECK(!kk_addr_map_publish(&map, 0x2000, 0x1000, BUF(2), "identical"),
         "an identical range must be refused");
   CHECK(!kk_addr_map_publish(&map, 0x2400, 0x400, BUF(2), "inside"),
         "a range strictly inside must be refused");
   CHECK(!kk_addr_map_publish(&map, 0x1000, 0x1800, BUF(2), "straddle low"),
         "a range straddling the low edge must be refused");
   CHECK(!kk_addr_map_publish(&map, 0x2800, 0x1000, BUF(2), "straddle high"),
         "a range straddling the high edge must be refused");
   CHECK(!kk_addr_map_publish(&map, 0x1000, 0x4000, BUF(2), "enclosing"),
         "an enclosing range must be refused");
   CHECK(map.count == 1, "no refused overlap may land (count %u)", map.count);
   CHECK(kk_addr_map_lookup(&map, 0x2abc, 0, &ref) && ref.buffer == BUF(1),
         "a refused overlap must not have disturbed the existing entry");

   /* Exact abutment is not overlap. */
   CHECK(kk_addr_map_publish(&map, 0x3000, 0x1000, BUF(3), "abuts above"),
         "a range starting exactly at the end of another must be accepted");
   CHECK(kk_addr_map_publish(&map, 0x1000, 0x1000, BUF(4), "abuts below"),
         "a range ending exactly at the base of another must be accepted");
   CHECK(kk_addr_map_lookup(&map, 0x2fff, 0, &ref) && ref.buffer == BUF(1),
         "abutment must not steal the neighbour's last byte");
   CHECK(kk_addr_map_lookup(&map, 0x3000, 0, &ref) && ref.buffer == BUF(3) &&
            ref.offset_B == 0,
         "the abutting range owns its own first byte");
   CHECK(kk_addr_map_lookup(&map, 0x1fff, 0, &ref) && ref.buffer == BUF(4) &&
            ref.offset_B == 0xfff,
         "the range below owns its own last byte");

   /* Withdraw. */
   kk_addr_map_withdraw(&map, 0x2000);
   CHECK(map.count == 2, "withdraw removes exactly one entry (count %u)",
         map.count);
   CHECK(!kk_addr_map_lookup(&map, 0x2abc, 0, &ref),
         "a withdrawn range no longer resolves");
   CHECK(kk_addr_map_lookup(&map, 0x3abc, 0, &ref) && ref.buffer == BUF(3),
         "withdraw leaves the entry above intact");
   CHECK(kk_addr_map_lookup(&map, 0x1abc, 0, &ref) && ref.buffer == BUF(4),
         "withdraw leaves the entry below intact");

   kk_addr_map_withdraw(&map, 0x2000);
   CHECK(map.count == 2, "withdrawing twice is a no-op (count %u)", map.count);
   kk_addr_map_withdraw(&map, 0x2abc);
   CHECK(map.count == 2,
         "withdraw keys on the base, not on containment (count %u)", map.count);
   kk_addr_map_withdraw(&map, 0);
   CHECK(map.count == 2, "withdrawing address 0 is a no-op (count %u)",
         map.count);

   /* Address recycling: Metal hands a released allocation's virtual address
    * straight back out, so the very next publish can legitimately reuse a base
    * that was just withdrawn — with a different buffer. */
   kk_addr_map_withdraw(&map, 0x3000);
   CHECK(kk_addr_map_publish(&map, 0x3000, 0x800, BUF(5), "recycled"),
         "a withdrawn base must be immediately republishable");
   CHECK(kk_addr_map_lookup(&map, 0x3100, 0, &ref) && ref.buffer == BUF(5),
         "a recycled address resolves to the NEW buffer");
   CHECK(!kk_addr_map_lookup(&map, 0x3900, 0, &ref),
         "the recycled entry is only as long as it says");

   kk_addr_map_finish(&map);
}

/* ---- 2. model check ----------------------------------------------------- */

#define MODEL_MAX 512

struct model_entry {
   uint64_t base, size_B;
   mtl_buffer *buffer;
   bool live;
};

static struct model_entry model[MODEL_MAX];

/* The reference implementation: a linear scan. Deliberately stupid. */
static const struct model_entry *
model_find(uint64_t addr)
{
   for (unsigned i = 0; i < MODEL_MAX; ++i) {
      if (model[i].live && addr >= model[i].base &&
          addr - model[i].base < model[i].size_B)
         return &model[i];
   }
   return NULL;
}

static bool
model_overlaps(uint64_t base, uint64_t size_B)
{
   for (unsigned i = 0; i < MODEL_MAX; ++i) {
      if (model[i].live && base < model[i].base + model[i].size_B &&
          model[i].base < base + size_B)
         return true;
   }
   return false;
}

/* Every invariant the binary search depends on, checked directly against the
 * array rather than through the API. */
static bool
map_is_well_formed(const struct kk_addr_map *map, unsigned expect_count)
{
   if (map->count != expect_count)
      return false;
   if (map->count > map->capacity)
      return false;

   for (uint32_t i = 0; i < map->count; ++i) {
      if (map->entries[i].size_B == 0 || map->entries[i].buffer == NULL)
         return false;
      if (i > 0 && map->entries[i - 1].base + map->entries[i - 1].size_B >
                      map->entries[i].base)
         return false;
   }
   return true;
}

static uint64_t rng_state = 0x243f6a8885a308d3ull;

static uint64_t
rng(void)
{
   rng_state ^= rng_state << 13;
   rng_state ^= rng_state >> 7;
   rng_state ^= rng_state << 17;
   return rng_state;
}

static void
test_model(unsigned iterations)
{
   struct kk_addr_map map;
   unsigned live = 0;
   unsigned published = 0, refused = 0, withdrawn = 0;
   unsigned hits = 0, misses = 0;
   bool structure_ok = true, agree = true;

   group("model check");
   kk_addr_map_init(&map);
   memset(model, 0, sizeof(model));

   for (unsigned it = 0; it < iterations; ++it) {
      const unsigned slot = (unsigned)(rng() % MODEL_MAX);
      const unsigned op = (unsigned)(rng() % 10u);

      if (op < 4u) {
         /* publish. Bases and sizes are deliberately tiny and dense so that
          * abutment, one-byte gaps and overlap all happen often. */
         if (model[slot].live)
            continue;

         const uint64_t base = 0x1000 + (rng() % 4096u) * 16u;
         const uint64_t size_B = 16u + (rng() % 64u) * 16u;
         const bool expect = !model_overlaps(base, size_B);
         const bool got =
            kk_addr_map_publish(&map, base, size_B, BUF(slot % 64u), "model");

         if (got != expect)
            agree = false;
         if (got) {
            model[slot] = (struct model_entry){base, size_B, BUF(slot % 64u),
                                               true};
            ++live;
            ++published;
         } else {
            ++refused;
         }
      } else if (op < 6u) {
         /* withdraw */
         if (!model[slot].live)
            continue;

         kk_addr_map_withdraw(&map, model[slot].base);
         model[slot].live = false;
         --live;
         ++withdrawn;
      } else {
         /* lookup, against the linear reference */
         const uint64_t addr = 0x1000 + (rng() % 70000u);
         const struct model_entry *want = model_find(addr);
         struct kk_addr_ref ref;
         const bool got = kk_addr_map_lookup(&map, addr, 0, &ref);

         if (want == NULL) {
            if (got)
               agree = false;
            ++misses;
         } else {
            if (!got || ref.buffer != want->buffer ||
                ref.offset_B != addr - want->base ||
                ref.size_B != want->size_B - (addr - want->base))
               agree = false;
            ++hits;
         }
         continue; /* no mutation, no structure check */
      }

      if (!map_is_well_formed(&map, live))
         structure_ok = false;
   }

   printf("  %u iterations: %u published, %u refused as overlapping, %u "
          "withdrawn; %u lookups hit, %u missed; peak %u entries\n",
          iterations, published, refused, withdrawn, hits, misses,
          map.peak_count);

   CHECK(agree, "every operation must agree with the linear reference model");
   CHECK(structure_ok,
         "the array must stay sorted, disjoint and correctly sized after every "
         "mutation");
   CHECK(hits > 1000 && misses > 1000,
          "the workload must actually exercise both hits (%u) and misses (%u)",
          hits, misses);
   CHECK(refused > 100, "the workload must actually exercise overlap refusal "
                        "(%u)",
         refused);

   kk_addr_map_finish(&map);
}

/* ---- 3. concurrency ----------------------------------------------------- */

/* Each mutator thread owns a private, disjoint slice of the address space and
 * churns it. Each reader repeatedly resolves addresses in a set of mappings that
 * are published once and never withdrawn, so every one of its lookups has a
 * known-correct answer no matter what the mutators are doing. A lock bug shows
 * up either as a wrong answer here or as a sanitizer report. */

#define THREAD_MUTATORS 4
#define THREAD_READERS 4
#define THREAD_ITERATIONS 20000
#define STABLE_COUNT 8

static struct kk_addr_map shared_map;
static uint64_t stable_base[STABLE_COUNT];
static const uint64_t stable_size = 0x1000;
static volatile int reader_errors;

static void *
mutator(void *arg)
{
   const uint64_t slice = 0x1000000ull * (1u + (uintptr_t)arg);
   uint64_t local_rng = 0x9e3779b97f4a7c15ull * (1u + (uintptr_t)arg);

   for (unsigned i = 0; i < THREAD_ITERATIONS; ++i) {
      local_rng ^= local_rng << 13;
      local_rng ^= local_rng >> 7;
      local_rng ^= local_rng << 17;

      const uint64_t base = slice + (local_rng % 256u) * 0x10000ull;

      /* Publish may be refused if this slot is still live from a previous
       * iteration; either way the withdraw below leaves the slice clean. */
      kk_addr_map_publish(&shared_map, base, 0x8000, BUF(i % 64u), "mutator");
      kk_addr_map_withdraw(&shared_map, base);
   }
   return NULL;
}

static void *
reader(void *arg)
{
   (void)arg;

   for (unsigned i = 0; i < THREAD_ITERATIONS; ++i) {
      const unsigned which = i % STABLE_COUNT;
      const uint64_t offset = (i * 64u) % stable_size;
      struct kk_addr_ref ref;

      if (!kk_addr_map_lookup(&shared_map, stable_base[which] + offset, 1,
                              &ref) ||
          ref.buffer != BUF(which) || ref.offset_B != offset)
         reader_errors = 1;
   }
   return NULL;
}

static void
test_threads(void)
{
   pthread_t t[THREAD_MUTATORS + THREAD_READERS];

   group("concurrency");
   kk_addr_map_init(&shared_map);

   for (unsigned i = 0; i < STABLE_COUNT; ++i) {
      stable_base[i] = 0x100000ull + i * 0x10000ull;
      if (!kk_addr_map_publish(&shared_map, stable_base[i], stable_size, BUF(i),
                               "stable"))
         CHECK(false, "failed to publish stable mapping %u", i);
   }

   for (unsigned i = 0; i < THREAD_MUTATORS; ++i)
      pthread_create(&t[i], NULL, mutator, (void *)(uintptr_t)i);
   for (unsigned i = 0; i < THREAD_READERS; ++i)
      pthread_create(&t[THREAD_MUTATORS + i], NULL, reader, NULL);
   for (unsigned i = 0; i < THREAD_MUTATORS + THREAD_READERS; ++i)
      pthread_join(t[i], NULL);

   printf("  %u mutator + %u reader threads x %u iterations, peak %u entries\n",
          THREAD_MUTATORS, THREAD_READERS, THREAD_ITERATIONS,
          shared_map.peak_count);

   CHECK(!reader_errors,
         "a concurrent lookup of a stable mapping returned the wrong answer");
   CHECK(shared_map.count == STABLE_COUNT,
         "the mutators must have cleaned up after themselves (%u entries left, "
         "expected %u)",
         shared_map.count, STABLE_COUNT);
   CHECK(map_is_well_formed(&shared_map, STABLE_COUNT),
         "the map must still be well formed after concurrent mutation");

   kk_addr_map_finish(&shared_map);
}

/* ---- 4. cost ------------------------------------------------------------ */

/* The header claims publish and lookup are cheap next to the Metal allocation
 * they sit beside. These are the numbers behind that claim; the Metal side is
 * measured by mtl-heap-addr-probe.m, which prints the cost of the
 * newHeapWithDescriptor: + whole-heap-buffer pair that every publish
 * accompanies. */

static double
now_ms(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static void
test_cost(void)
{
   enum { N = 4096, LOOKUPS = 100000 };
   struct kk_addr_map map;
   double t0, publish_ms, lookup_ms, withdraw_ms;

   group("cost");
   kk_addr_map_init(&map);

   /* Descending bases, so every insert lands at index 0 and memmoves the entire
    * tail. This is the worst case, not the average. */
   t0 = now_ms();
   for (unsigned i = 0; i < N; ++i)
      kk_addr_map_publish(&map, 0x100000ull + (uint64_t)(N - 1 - i) * 0x10000ull,
                          0x8000, BUF(i % 64u), "cost");
   publish_ms = now_ms() - t0;

   CHECK(map.count == N, "the cost workload published %u of %u", map.count, N);

   t0 = now_ms();
   uint64_t found = 0;
   for (unsigned i = 0; i < LOOKUPS; ++i) {
      struct kk_addr_ref ref;
      if (kk_addr_map_lookup(&map, 0x100000ull + (uint64_t)(i % N) * 0x10000ull,
                             1, &ref))
         found += ref.offset_B + 1u;
   }
   lookup_ms = now_ms() - t0;
   CHECK(found == LOOKUPS, "every cost-workload lookup should have hit");

   t0 = now_ms();
   for (unsigned i = 0; i < N; ++i)
      kk_addr_map_withdraw(&map, 0x100000ull + (uint64_t)i * 0x10000ull);
   withdraw_ms = now_ms() - t0;

   CHECK(map.count == 0, "the cost workload left %u entries behind", map.count);

   printf("  publish  %u entries worst case (each insert at index 0): %.3f ms "
          "total, %.4f us each\n",
          N, publish_ms, publish_ms * 1000.0 / N);
   printf("  lookup   %u times in a %u-entry map: %.3f ms total, %.4f us each\n",
          LOOKUPS, N, lookup_ms, lookup_ms * 1000.0 / LOOKUPS);
   printf("  withdraw %u entries worst case (each from index 0): %.3f ms "
          "total, %.4f us each\n",
          N, withdraw_ms, withdraw_ms * 1000.0 / N);

   kk_addr_map_finish(&map);
}

int
main(int argc, char **argv)
{
   const bool quick = argc > 1 && strcmp(argv[1], "--quick") == 0;

   test_edges();
   test_model(quick ? 20000u : 400000u);
   test_threads();
   if (!quick)
      test_cost();

   printf("\n%u/%u checks passed\n", pass_count, pass_count + fail_count);
   return fail_count == 0 ? 0 : 1;
}
