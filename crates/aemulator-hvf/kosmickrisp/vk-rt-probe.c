/* vk-rt-probe.c — a native arm64 Android Vulkan ray-tracing conformance probe.
 *
 * WHAT THIS IS FOR
 * ----------------
 * An extension list is a claim. A rendered intersection is evidence. This
 * program is our own oracle for "does ray tracing work in the AEmulator guest":
 * no app framework, no assets, no Java, one static-linked-nothing executable
 * that walks the whole path
 *
 *    guest Android libvulkan.so loader
 *      -> vulkan.ranchu.so (mesa gfxstream guest ICD)
 *      -> virtio-gpu capset 3
 *      -> host gfxstream renderer
 *      -> KosmicKrisp (Vulkan-on-Metal)
 *      -> Metal on the Apple GPU
 *
 * and reports, stage by stage, exactly where it stops. It asks the GPU the SAME
 * question as the host-side crates/aemulator-hvf/kosmickrisp/rq-smoke.c (same
 * shader, same triangle, same rays), so a host PASS next to a guest FAIL
 * localises the bug to the transport rather than to the driver.
 *
 * THE ANSWER IT PRODUCES
 * ----------------------
 * Eight rays are shot down +Z from (0.25 + i, 0.25, 0). The BLAS holds the one
 * triangle (0,0,1) (1,0,1) (0,1,1), so:
 *      ray 0 MUST hit  at t = 1, prim 0, geom 0, instance 0, bary (0.25, 0.25)
 *      rays 1..7 MUST miss
 * Both halves are required. A driver that hits everything and a driver that
 * misses everything are equally broken, and a hit-only test cannot tell them
 * apart. The last line is one of
 *      VK_RT_PROBE: PASS
 *      VK_RT_PROBE: FAIL at <stage>
 * and the exit status is 0 only for PASS.
 *
 * WHAT THIS PROBE CURRENTLY MEASURES (as of 2026-07-29)
 * ----------------------------------------------------
 * It FAILS at stage ray-hit, and that failure is now specific to the guest.
 *
 * History, because it changes how the failure should be read. On an Apple M2
 * (GPU family Apple8) Metal has no hardware ray tracing and services it with a
 * software BVH, and the Metal 4 ray-tracing surface is not implemented against
 * that emulated builder: a structure built through
 * -[MTL4ComputeCommandEncoder buildAccelerationStructure:...] completes without
 * error and then intersects nothing. That WAS the whole story, and it has since
 * been fixed — KosmicKrisp builds through the legacy
 * MTLAccelerationStructureCommandEncoder on such a device
 * (0012-kosmickrisp-legacy-acceleration-structure-builder.patch), and the
 * host-side twin of this probe, kosmickrisp/rq-smoke.c, now scores 16/16 with a
 * real intersection on this same M2.
 *
 * So a ray-hit failure here no longer points at the builder. The host hits; the
 * guest does not. With MESA_KK_RT_DEBUG=1 the renderer shows that during the
 * guest's build it selects the legacy builder, creates the legacy queue for the
 * guest's VkDevice, builds both BLAS and TLAS through it with "descriptor ok",
 * and never reports an unresolvable device address — the same lines the passing
 * host run prints. The remaining suspects are therefore the geometry parameters
 * as gfxstream's cereal marshals them for vkCmdBuildAccelerationStructuresKHR
 * (the T* const* ppGeometries indirection) and the ordering of the event-split
 * submission when the host replays the guest's command buffer. This probe is the
 * tool that will prove that fix when it lands: same binary, same output, one
 * stage flipping from FAIL to PASS.
 *
 * LINKAGE
 * -------
 * The only Vulkan symbol this binary imports is vkGetInstanceProcAddr, from the
 * guest's own /system/lib64/libvulkan.so. Everything else is resolved at run
 * time through vkGetInstanceProcAddr / vkGetDeviceProcAddr. No loader is
 * bundled or static-linked. That is deliberate: a guest loader that is missing
 * a Vulkan 1.2 core export would otherwise turn into a dlopen failure with no
 * diagnostic, whereas here it turns into a named missing entry point.
 *
 * The SPIR-V is compiled from vk-rt-probe.comp at BUILD time and embedded by
 * build-vk-rt-probe.sh as vk-rt-probe-spv.h, so the probe needs no files on the
 * guest beyond itself.
 */

#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include <math.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "vk-rt-probe-spv.h"

/* ------------------------------------------------------------------------- */
/* Stage tracking. Every stage is named, and the name is what the final line   */
/* reports, so a failure is localised without reading the rest of the output.  */
/* ------------------------------------------------------------------------- */

static const char *cur_stage = "startup";
static int failed;

static void
stage(const char *name)
{
   cur_stage = name;
   printf("\n== stage: %s ==\n", name);
}

/* Record a failure but keep going where continuing still yields information.
 * The first failing stage is the one reported, because later stages are
 * usually just its consequences. */
static const char *first_failure;

static void
fail(const char *fmt, ...)
{
   va_list ap;
   failed = 1;
   if (!first_failure)
      first_failure = cur_stage;
   printf("  FAIL: ");
   va_start(ap, fmt);
   vprintf(fmt, ap);
   va_end(ap);
   printf("\n");
}

static void
ok(const char *fmt, ...)
{
   va_list ap;
   printf("  ok: ");
   va_start(ap, fmt);
   vprintf(fmt, ap);
   va_end(ap);
   printf("\n");
}

static void
info(const char *fmt, ...)
{
   va_list ap;
   printf("  ");
   va_start(ap, fmt);
   vprintf(fmt, ap);
   va_end(ap);
   printf("\n");
}

/* Print the verdict and leave. Used for the unrecoverable stages: there is no
 * point building a TLAS if the device could not be created. */
static int
verdict(void)
{
   printf("\n");
   if (!failed) {
      printf("VK_RT_PROBE: PASS\n");
      fflush(stdout);
      return 0;
   }
   printf("VK_RT_PROBE: FAIL at %s\n", first_failure ? first_failure : cur_stage);
   fflush(stdout);
   return 1;
}

/* A crash below the probe (guest ICD, transport, host renderer) must not look
 * like silence. Name the stage and re-raise so the shell still sees the signal. */
static void
on_fatal_signal(int sig)
{
   const char *n = sig == SIGSEGV ? "SIGSEGV"
                   : sig == SIGBUS ? "SIGBUS"
                   : sig == SIGABRT ? "SIGABRT"
                   : sig == SIGILL ? "SIGILL"
                                   : "signal";
   /* write(2), not printf: async-signal-safe, and stdout is unbuffered anyway. */
   char buf[160];
   int n2 = snprintf(buf, sizeof buf,
                     "\n%s inside stage '%s'\nVK_RT_PROBE: FAIL at %s (%s)\n", n,
                     cur_stage, cur_stage, n);
   if (n2 > 0)
      (void)!write(STDOUT_FILENO, buf, (size_t)n2);
   signal(sig, SIG_DFL);
   raise(sig);
}

/* ------------------------------------------------------------------------- */
/* Entry points. With VK_NO_PROTOTYPES the Vulkan names are free, so the        */
/* function pointers are given those exact names and the body below reads like  */
/* ordinary Vulkan code.                                                       */
/* ------------------------------------------------------------------------- */

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetInstanceProcAddr(VkInstance,
                                                               const char *);

#define GLOBAL_FUNCS(F)                                                        \
   F(vkCreateInstance)                                                         \
   F(vkEnumerateInstanceVersion)                                               \
   F(vkEnumerateInstanceExtensionProperties)

/* Only what is actually called. A loaded-but-unused entry point could fail the
 * rt-entrypoints stage for no reason, and the instance/device are deliberately
 * not destroyed — the per-test objects are torn down explicitly (a crash there
 * is a finding) and process exit reclaims the rest, exactly as the host-side
 * rq-smoke.c does, so the two are comparable. */
#define INSTANCE_FUNCS(F)                                                      \
   F(vkEnumeratePhysicalDevices)                                               \
   F(vkGetPhysicalDeviceProperties)                                            \
   F(vkGetPhysicalDeviceProperties2)                                           \
   F(vkGetPhysicalDeviceFeatures2)                                             \
   F(vkGetPhysicalDeviceMemoryProperties)                                      \
   F(vkGetPhysicalDeviceQueueFamilyProperties)                                 \
   F(vkEnumerateDeviceExtensionProperties)                                     \
   F(vkCreateDevice)                                                           \
   F(vkGetDeviceProcAddr)

#define DEVICE_FUNCS(F)                                                        \
   F(vkDeviceWaitIdle)                                                         \
   F(vkGetDeviceQueue)                                                         \
   F(vkCreateBuffer)                                                           \
   F(vkGetBufferMemoryRequirements)                                            \
   F(vkAllocateMemory)                                                         \
   F(vkBindBufferMemory)                                                       \
   F(vkMapMemory)                                                              \
   F(vkFlushMappedMemoryRanges)                                                \
   F(vkInvalidateMappedMemoryRanges)                                           \
   F(vkCreateCommandPool)                                                      \
   F(vkDestroyCommandPool)                                                     \
   F(vkAllocateCommandBuffers)                                                 \
   F(vkBeginCommandBuffer)                                                     \
   F(vkEndCommandBuffer)                                                       \
   F(vkQueueSubmit)                                                            \
   F(vkCreateFence)                                                            \
   F(vkDestroyFence)                                                           \
   F(vkWaitForFences)                                                          \
   F(vkCmdPipelineBarrier)                                                     \
   F(vkCreateDescriptorSetLayout)                                              \
   F(vkDestroyDescriptorSetLayout)                                             \
   F(vkCreateDescriptorPool)                                                   \
   F(vkDestroyDescriptorPool)                                                  \
   F(vkAllocateDescriptorSets)                                                 \
   F(vkUpdateDescriptorSets)                                                   \
   F(vkCreateShaderModule)                                                     \
   F(vkDestroyShaderModule)                                                    \
   F(vkCreatePipelineLayout)                                                   \
   F(vkDestroyPipelineLayout)                                                  \
   F(vkCreateComputePipelines)                                                 \
   F(vkDestroyPipeline)                                                        \
   F(vkCmdBindPipeline)                                                        \
   F(vkCmdBindDescriptorSets)                                                  \
   F(vkCmdDispatch)

/* The ray-tracing entry points, plus vkGetBufferDeviceAddress, which the whole
 * acceleration-structure API is built on. Each is loaded by name and checked,
 * because a NULL here is the single most informative failure this probe can
 * report: the extension was advertised but the entry point is not plumbed. */
#define RT_FUNCS(F)                                                            \
   F(vkGetAccelerationStructureBuildSizesKHR)                                  \
   F(vkCreateAccelerationStructureKHR)                                         \
   F(vkDestroyAccelerationStructureKHR)                                        \
   F(vkGetAccelerationStructureDeviceAddressKHR)                               \
   F(vkCmdBuildAccelerationStructuresKHR)

#define DECL(n) static PFN_##n n;
GLOBAL_FUNCS(DECL)
INSTANCE_FUNCS(DECL)
DEVICE_FUNCS(DECL)
RT_FUNCS(DECL)
#undef DECL

/* vkGetBufferDeviceAddress is core in Vulkan 1.2 but the KHR alias is what a
 * 1.1 driver exposes, so it is resolved separately with a fallback. */
static PFN_vkGetBufferDeviceAddress vkGetBufferDeviceAddress;

static int missing_count;

static void
note_missing(const char *name)
{
   missing_count++;
   fail("entry point %s is NULL", name);
}

/* ------------------------------------------------------------------------- */
/* Buffers                                                                    */
/* ------------------------------------------------------------------------- */

static VkDevice dev;
static VkPhysicalDevice pdev;
static VkPhysicalDeviceMemoryProperties memprops;

/* Resolve a device-level function, preferring vkGetDeviceProcAddr.
 *
 * The fallback to vkGetInstanceProcAddr is here for one specific reason: the
 * Android loader answers vkGetDeviceProcAddr from its own table of known
 * functions and only forwards names it does not recognise to the driver. If a
 * loader ever fails to forward the ray-tracing entry points, the extension is
 * advertised, the device is created, and then every RT call is a NULL
 * dereference with nothing to explain it. Trying the instance path second
 * turns that into a printed line naming the loader as the culprit — which is a
 * different bug from the driver not implementing the function, and the two must
 * not be conflated. */
static PFN_vkVoidFunction
resolve_device_fn(VkInstance instance, const char *name)
{
   PFN_vkVoidFunction f = vkGetDeviceProcAddr(dev, name);
   if (f)
      return f;

   f = vkGetInstanceProcAddr(instance, name);
   if (f)
      info("NOTE: %s resolved only via vkGetInstanceProcAddr — "
           "vkGetDeviceProcAddr returned NULL, so the loader is not forwarding "
           "it to the driver",
           name);
   return f;
}

struct buf {
   VkBuffer buffer;
   VkDeviceMemory mem;
   VkDeviceAddress addr;
   void *map;          /* NULL when the memory is not host visible */
   int coherent;
   VkDeviceSize size;
};

/* Pick a memory type. want_map buffers must be HOST_VISIBLE; the rest prefer
 * DEVICE_LOCAL but will take anything the buffer allows. Reporting the chosen
 * type is worth the line: an acceleration structure landing in host-visible
 * memory on the guest is a fact worth having when a build misbehaves. */
static uint32_t
pick_memory_type(uint32_t allowed, int want_map, int *out_coherent)
{
   uint32_t best = UINT32_MAX;
   *out_coherent = 0;

   for (uint32_t i = 0; i < memprops.memoryTypeCount; ++i) {
      if (!(allowed & (1u << i)))
         continue;
      VkMemoryPropertyFlags f = memprops.memoryTypes[i].propertyFlags;

      if (want_map) {
         if (!(f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))
            continue;
         if (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) {
            *out_coherent = 1;
            return i;   /* host visible + coherent is exactly what we want */
         }
         if (best == UINT32_MAX)
            best = i;   /* host visible but needs explicit flush/invalidate */
      } else {
         if (f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)
            return i;
         if (best == UINT32_MAX)
            best = i;
      }
   }
   return best;
}

static int
make_buf(struct buf *b, VkDeviceSize size, VkBufferUsageFlags usage, int want_map,
         const char *what)
{
   VkResult r;

   memset(b, 0, sizeof *b);
   b->size = size;

   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = usage | VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   r = vkCreateBuffer(dev, &bci, NULL, &b->buffer);
   if (r != VK_SUCCESS) {
      fail("vkCreateBuffer(%s, %llu bytes) -> %d", what,
           (unsigned long long)size, r);
      return 0;
   }

   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, b->buffer, &req);

   uint32_t type = pick_memory_type(req.memoryTypeBits, want_map, &b->coherent);
   if (type == UINT32_MAX) {
      fail("no %s memory type for %s (memoryTypeBits 0x%x)",
           want_map ? "host visible" : "usable", what, req.memoryTypeBits);
      return 0;
   }

   VkMemoryAllocateFlagsInfo flags = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
      .flags = VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT,
   };
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &flags,
      .allocationSize = req.size,
      .memoryTypeIndex = type,
   };
   r = vkAllocateMemory(dev, &ai, NULL, &b->mem);
   if (r != VK_SUCCESS) {
      fail("vkAllocateMemory(%s, %llu bytes, type %u) -> %d", what,
           (unsigned long long)req.size, type, r);
      return 0;
   }
   r = vkBindBufferMemory(dev, b->buffer, b->mem, 0);
   if (r != VK_SUCCESS) {
      fail("vkBindBufferMemory(%s) -> %d", what, r);
      return 0;
   }

   if (want_map) {
      r = vkMapMemory(dev, b->mem, 0, VK_WHOLE_SIZE, 0, &b->map);
      if (r != VK_SUCCESS) {
         fail("vkMapMemory(%s) -> %d", what, r);
         return 0;
      }
   }

   VkBufferDeviceAddressInfo bdai = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
      .buffer = b->buffer,
   };
   b->addr = vkGetBufferDeviceAddress(dev, &bdai);
   if (b->addr == 0) {
      fail("vkGetBufferDeviceAddress(%s) returned 0", what);
      return 0;
   }

   info("%-14s %8llu bytes  memtype %u (flags 0x%x)%s  addr 0x%llx", what,
        (unsigned long long)size, type,
        memprops.memoryTypes[type].propertyFlags,
        want_map ? (b->coherent ? " mapped/coherent" : " mapped/non-coherent")
                 : "",
        (unsigned long long)b->addr);
   return 1;
}

static void
buf_flush(struct buf *b)
{
   if (!b->map || b->coherent)
      return;
   VkMappedMemoryRange range = {
      .sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
      .memory = b->mem,
      .offset = 0,
      .size = VK_WHOLE_SIZE,
   };
   vkFlushMappedMemoryRanges(dev, 1, &range);
}

static void
buf_invalidate(struct buf *b)
{
   if (!b->map || b->coherent)
      return;
   VkMappedMemoryRange range = {
      .sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
      .memory = b->mem,
      .offset = 0,
      .size = VK_WHOLE_SIZE,
   };
   vkInvalidateMappedMemoryRanges(dev, 1, &range);
}

/* ------------------------------------------------------------------------- */
/* Must match Result in vk-rt-probe.comp (std430, all 4-byte scalars).                  */
/* ------------------------------------------------------------------------- */

struct result {
   uint32_t committed_type;
   float t;
   uint32_t prim;
   uint32_t geom;
   uint32_t instance_id;
   uint32_t custom_index;
   float bary_x;
   float bary_y;
   uint32_t front_face;
   uint32_t candidates_seen;
   float ray_tmin;
   uint32_t ray_flags;
};

#define NRAYS 8
#define CUSTOM_INDEX 0x123456u

static int
near_eq(float a, float b)
{
   return fabsf(a - b) < 1e-4f;
}

static VkDeviceAddress
align_up(VkDeviceAddress v, VkDeviceAddress a)
{
   return a <= 1 ? v : ((v + a - 1) / a) * a;
}

/* ------------------------------------------------------------------------- */

int
main(int argc, char **argv)
{
   int list_exts = 1;
   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "-q"))
         list_exts = 0;
      else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
         printf("usage: vk-rt-probe [-q]\n"
                "  -q  do not dump the full device extension list\n");
         return 0;
      }
   }

   /* Unbuffered: if the guest transport takes the process down mid-stage we
    * still want every line printed up to that point. This has bitten before. */
   setvbuf(stdout, NULL, _IONBF, 0);
   signal(SIGSEGV, on_fatal_signal);
   signal(SIGBUS, on_fatal_signal);
   signal(SIGABRT, on_fatal_signal);
   signal(SIGILL, on_fatal_signal);

   printf("vk-rt-probe — native arm64 Vulkan ray-query conformance probe\n");
   printf("built against Vulkan headers %u.%u.%u\n",
          VK_API_VERSION_MAJOR(VK_HEADER_VERSION_COMPLETE),
          VK_API_VERSION_MINOR(VK_HEADER_VERSION_COMPLETE),
          VK_API_VERSION_PATCH(VK_HEADER_VERSION_COMPLETE));

   /* ------------------------------------------------------------------ */
   stage("instance");
   /* ------------------------------------------------------------------ */

#define LOAD(inst, n)                                                          \
   do {                                                                        \
      n = (PFN_##n)vkGetInstanceProcAddr(inst, #n);                            \
      if (!n)                                                                  \
         note_missing(#n);                                                     \
   } while (0)

   /* vkEnumerateInstanceVersion is optional on a 1.0 loader, so it is loaded
    * without complaint. Everything else here is mandatory. */
   vkEnumerateInstanceVersion = (PFN_vkEnumerateInstanceVersion)
      vkGetInstanceProcAddr(NULL, "vkEnumerateInstanceVersion");
   vkEnumerateInstanceExtensionProperties =
      (PFN_vkEnumerateInstanceExtensionProperties)vkGetInstanceProcAddr(
         NULL, "vkEnumerateInstanceExtensionProperties");
   LOAD(NULL, vkCreateInstance);
   if (missing_count)
      return verdict();

   uint32_t loader_version = VK_API_VERSION_1_0;
   if (vkEnumerateInstanceVersion &&
       vkEnumerateInstanceVersion(&loader_version) != VK_SUCCESS)
      loader_version = VK_API_VERSION_1_0;
   info("loader instance version %u.%u.%u",
        VK_API_VERSION_MAJOR(loader_version),
        VK_API_VERSION_MINOR(loader_version),
        VK_API_VERSION_PATCH(loader_version));

   /* Ask for the highest instance version the loader admits to, capped at the
    * 1.3 this probe was written against. Asking for more than the loader
    * supports is what VK_ERROR_INCOMPATIBLE_DRIVER is for. */
   uint32_t want_api = loader_version < VK_API_VERSION_1_3 ? loader_version
                                                           : VK_API_VERSION_1_3;
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vk-rt-probe",
      .apiVersion = want_api,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst = VK_NULL_HANDLE;
   VkResult r = vkCreateInstance(&ici, NULL, &inst);
   if (r != VK_SUCCESS) {
      fail("vkCreateInstance(apiVersion %u.%u) -> %d",
           VK_API_VERSION_MAJOR(want_api), VK_API_VERSION_MINOR(want_api), r);
      return verdict();
   }
   ok("vkCreateInstance -> VK_SUCCESS (requested %u.%u)",
      VK_API_VERSION_MAJOR(want_api), VK_API_VERSION_MINOR(want_api));

#define LOAD_I(n) LOAD(inst, n);
   INSTANCE_FUNCS(LOAD_I)
#undef LOAD_I
   if (missing_count)
      return verdict();

   /* ------------------------------------------------------------------ */
   stage("physical-device");
   /* ------------------------------------------------------------------ */

   uint32_t ndev = 0;
   r = vkEnumeratePhysicalDevices(inst, &ndev, NULL);
   if (r != VK_SUCCESS || ndev == 0) {
      fail("vkEnumeratePhysicalDevices -> %d, count %u", r, ndev);
      return verdict();
   }
   VkPhysicalDevice *pdevs = calloc(ndev, sizeof *pdevs);
   if (!pdevs) {
      fail("out of memory");
      return verdict();
   }
   r = vkEnumeratePhysicalDevices(inst, &ndev, pdevs);
   if (r != VK_SUCCESS) {
      fail("vkEnumeratePhysicalDevices (fill) -> %d", r);
      return verdict();
   }
   info("%u physical device(s)", ndev);
   pdev = pdevs[0];

   VkPhysicalDeviceDriverProperties drvprops = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
   };
   VkPhysicalDeviceProperties2 props2 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
      .pNext = &drvprops,
   };
   vkGetPhysicalDeviceProperties2(pdev, &props2);
   VkPhysicalDeviceProperties *p = &props2.properties;

   for (uint32_t i = 0; i < ndev; i++) {
      VkPhysicalDeviceProperties pi;
      vkGetPhysicalDeviceProperties(pdevs[i], &pi);
      info("  [%u]%s %s", i, i == 0 ? " (chosen)" : "        ", pi.deviceName);
   }
   free(pdevs);

   info("deviceName    %s", p->deviceName);
   info("apiVersion    %u.%u.%u", VK_API_VERSION_MAJOR(p->apiVersion),
        VK_API_VERSION_MINOR(p->apiVersion),
        VK_API_VERSION_PATCH(p->apiVersion));
   info("driverVersion 0x%08x", p->driverVersion);
   info("vendorID      0x%04x  deviceID 0x%04x  type %u", p->vendorID,
        p->deviceID, p->deviceType);
   info("driverName    %s", drvprops.driverName[0] ? drvprops.driverName : "(none)");
   info("driverInfo    %s", drvprops.driverInfo[0] ? drvprops.driverInfo : "(none)");
   info("driverID      %u  conformance %u.%u.%u.%u", drvprops.driverID,
        drvprops.conformanceVersion.major, drvprops.conformanceVersion.minor,
        drvprops.conformanceVersion.subminor,
        drvprops.conformanceVersion.patch);
   ok("physical device selected");

   vkGetPhysicalDeviceMemoryProperties(pdev, &memprops);

   /* ------------------------------------------------------------------ */
   stage("device-extensions");
   /* ------------------------------------------------------------------ */

   uint32_t next = 0;
   vkEnumerateDeviceExtensionProperties(pdev, NULL, &next, NULL);
   VkExtensionProperties *exts = calloc(next ? next : 1, sizeof *exts);
   if (!exts) {
      fail("out of memory");
      return verdict();
   }
   vkEnumerateDeviceExtensionProperties(pdev, NULL, &next, exts);
   info("%u device extension(s)", next);

   int has_as = 0, has_rq = 0, has_dho = 0, has_rtp = 0;
   int has_bda_ext = 0, has_di_ext = 0, has_spirv14 = 0;
   for (uint32_t i = 0; i < next; ++i) {
      const char *n = exts[i].extensionName;
      if (!strcmp(n, VK_KHR_ACCELERATION_STRUCTURE_EXTENSION_NAME))
         has_as = 1;
      else if (!strcmp(n, VK_KHR_RAY_QUERY_EXTENSION_NAME))
         has_rq = 1;
      else if (!strcmp(n, VK_KHR_DEFERRED_HOST_OPERATIONS_EXTENSION_NAME))
         has_dho = 1;
      else if (!strcmp(n, VK_KHR_RAY_TRACING_PIPELINE_EXTENSION_NAME))
         has_rtp = 1;
      else if (!strcmp(n, VK_KHR_BUFFER_DEVICE_ADDRESS_EXTENSION_NAME))
         has_bda_ext = 1;
      else if (!strcmp(n, VK_EXT_DESCRIPTOR_INDEXING_EXTENSION_NAME))
         has_di_ext = 1;
      else if (!strcmp(n, VK_KHR_SPIRV_1_4_EXTENSION_NAME))
         has_spirv14 = 1;
   }
   if (list_exts) {
      for (uint32_t i = 0; i < next; ++i)
         printf("    %-52s rev %u\n", exts[i].extensionName,
                exts[i].specVersion);
   }
   free(exts);

   printf("\n  the four extensions this probe is about:\n");
   info("  %-42s %s", VK_KHR_ACCELERATION_STRUCTURE_EXTENSION_NAME,
        has_as ? "PRESENT" : "absent");
   info("  %-42s %s", VK_KHR_RAY_QUERY_EXTENSION_NAME,
        has_rq ? "PRESENT" : "absent");
   info("  %-42s %s", VK_KHR_DEFERRED_HOST_OPERATIONS_EXTENSION_NAME,
        has_dho ? "PRESENT" : "absent");
   info("  %-42s %s", VK_KHR_RAY_TRACING_PIPELINE_EXTENSION_NAME,
        has_rtp ? "PRESENT" : "absent  (expected: Metal has no shader binding "
                              "table, ray query does not need it)");

   /* Ray query needs acceleration_structure, which needs
    * deferred_host_operations. ray_tracing_pipeline is genuinely optional for
    * inline ray tracing and its absence is not a failure. */
   if (!has_as || !has_rq || !has_dho) {
      fail("the required trio is incomplete: as=%d rq=%d dho=%d", has_as, has_rq,
           has_dho);
      return verdict();
   }
   ok("acceleration_structure + ray_query + deferred_host_operations all "
      "advertised");

   /* ------------------------------------------------------------------ */
   stage("features");
   /* ------------------------------------------------------------------ */

   VkPhysicalDeviceRayQueryFeaturesKHR rq_have = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_QUERY_FEATURES_KHR,
   };
   VkPhysicalDeviceAccelerationStructureFeaturesKHR as_have = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR,
      .pNext = &rq_have,
   };
   VkPhysicalDeviceBufferDeviceAddressFeatures bda_have = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES,
      .pNext = &as_have,
   };
   VkPhysicalDeviceFeatures2 feat_have = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
      .pNext = &bda_have,
   };
   vkGetPhysicalDeviceFeatures2(pdev, &feat_have);

   info("accelerationStructure                            %u",
        as_have.accelerationStructure);
   info("accelerationStructureCaptureReplay               %u",
        as_have.accelerationStructureCaptureReplay);
   info("accelerationStructureHostCommands                %u",
        as_have.accelerationStructureHostCommands);
   info("descriptorBindingAccelerationStructureUpdateAfterBind %u",
        as_have.descriptorBindingAccelerationStructureUpdateAfterBind);
   info("rayQuery                                         %u",
        rq_have.rayQuery);
   info("bufferDeviceAddress                              %u",
        bda_have.bufferDeviceAddress);

   if (!as_have.accelerationStructure || !rq_have.rayQuery ||
       !bda_have.bufferDeviceAddress) {
      fail("a required feature bit is 0 (accelerationStructure=%u rayQuery=%u "
           "bufferDeviceAddress=%u)",
           as_have.accelerationStructure, rq_have.rayQuery,
           bda_have.bufferDeviceAddress);
      return verdict();
   }
   ok("accelerationStructure, rayQuery and bufferDeviceAddress all reported "
      "supported");

   /* The acceleration-structure limits, for the record. */
   VkPhysicalDeviceAccelerationStructurePropertiesKHR asprops = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_PROPERTIES_KHR,
   };
   VkPhysicalDeviceProperties2 props2b = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
      .pNext = &asprops,
   };
   vkGetPhysicalDeviceProperties2(pdev, &props2b);
   info("maxGeometryCount %llu  maxInstanceCount %llu  maxPrimitiveCount %llu",
        (unsigned long long)asprops.maxGeometryCount,
        (unsigned long long)asprops.maxInstanceCount,
        (unsigned long long)asprops.maxPrimitiveCount);
   info("minAccelerationStructureScratchOffsetAlignment %u",
        asprops.minAccelerationStructureScratchOffsetAlignment);
   VkDeviceSize scratch_align =
      asprops.minAccelerationStructureScratchOffsetAlignment
         ? asprops.minAccelerationStructureScratchOffsetAlignment
         : 256;

   /* ------------------------------------------------------------------ */
   stage("device-create");
   /* ------------------------------------------------------------------ */

   uint32_t nqf = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, NULL);
   VkQueueFamilyProperties *qf = calloc(nqf ? nqf : 1, sizeof *qf);
   if (!qf) {
      fail("out of memory");
      return verdict();
   }
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, qf);
   uint32_t qfi = UINT32_MAX;
   for (uint32_t i = 0; i < nqf; i++) {
      info("queue family %u: flags 0x%x count %u", i, qf[i].queueFlags,
           qf[i].queueCount);
      if (qfi == UINT32_MAX && (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT))
         qfi = i;
   }
   free(qf);
   if (qfi == UINT32_MAX) {
      fail("no queue family with VK_QUEUE_COMPUTE_BIT");
      return verdict();
   }
   info("using queue family %u", qfi);

   /* Enable exactly the three extensions ray query needs, plus the dependencies
    * that are only extensions on a pre-1.2 device. Nothing else: this keeps a
    * VK_ERROR_EXTENSION_NOT_PRESENT / VK_ERROR_FEATURE_NOT_PRESENT pointing at
    * something specific instead of at a pile of unrelated requests. */
   const char *devexts[8];
   uint32_t ndevexts = 0;
   devexts[ndevexts++] = VK_KHR_DEFERRED_HOST_OPERATIONS_EXTENSION_NAME;
   devexts[ndevexts++] = VK_KHR_ACCELERATION_STRUCTURE_EXTENSION_NAME;
   devexts[ndevexts++] = VK_KHR_RAY_QUERY_EXTENSION_NAME;
   if (p->apiVersion < VK_API_VERSION_1_2) {
      /* buffer_device_address, descriptor_indexing and SPIR-V 1.4 became core
       * in 1.2; below that VK_KHR_acceleration_structure requires them by
       * name. */
      if (has_bda_ext)
         devexts[ndevexts++] = VK_KHR_BUFFER_DEVICE_ADDRESS_EXTENSION_NAME;
      if (has_di_ext)
         devexts[ndevexts++] = VK_EXT_DESCRIPTOR_INDEXING_EXTENSION_NAME;
      if (has_spirv14)
         devexts[ndevexts++] = VK_KHR_SPIRV_1_4_EXTENSION_NAME;
   }
   for (uint32_t i = 0; i < ndevexts; i++)
      info("enabling %s", devexts[i]);

   /* Fresh, minimal feature structs — not the queried chain fed back in. Only
    * the three bits this probe actually depends on are requested. */
   VkPhysicalDeviceRayQueryFeaturesKHR rq_want = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_QUERY_FEATURES_KHR,
      .rayQuery = VK_TRUE,
   };
   VkPhysicalDeviceAccelerationStructureFeaturesKHR as_want = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR,
      .pNext = &rq_want,
      .accelerationStructure = VK_TRUE,
   };
   VkPhysicalDeviceBufferDeviceAddressFeatures bda_want = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES,
      .pNext = &as_want,
      .bufferDeviceAddress = VK_TRUE,
   };
   VkPhysicalDeviceFeatures2 feat_want = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
      .pNext = &bda_want,
   };

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfi,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &feat_want,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = ndevexts,
      .ppEnabledExtensionNames = devexts,
   };
   r = vkCreateDevice(pdev, &dci, NULL, &dev);
   /* The exact VkResult is the deliverable of this stage. */
   info("vkCreateDevice -> %d%s", r,
        r == VK_SUCCESS ? " (VK_SUCCESS)"
        : r == VK_ERROR_EXTENSION_NOT_PRESENT
           ? " (VK_ERROR_EXTENSION_NOT_PRESENT — an enabled extension or one of "
             "its dependencies is not really there)"
        : r == VK_ERROR_FEATURE_NOT_PRESENT
           ? " (VK_ERROR_FEATURE_NOT_PRESENT — a requested feature bit was "
             "refused)"
        : r == VK_ERROR_INITIALIZATION_FAILED ? " (VK_ERROR_INITIALIZATION_FAILED)"
                                              : "");
   if (r != VK_SUCCESS) {
      fail("vkCreateDevice -> %d", r);
      return verdict();
   }
   ok("device created with acceleration_structure + ray_query enabled");

   /* ------------------------------------------------------------------ */
   stage("rt-entrypoints");
   /* ------------------------------------------------------------------ */

#define LOAD_D(n)                                                              \
   do {                                                                        \
      n = (PFN_##n)resolve_device_fn(inst, #n);                                \
      if (!n)                                                                  \
         note_missing(#n);                                                     \
   } while (0);
   DEVICE_FUNCS(LOAD_D)
   RT_FUNCS(LOAD_D)
#undef LOAD_D

   /* Core in Vulkan 1.2, KHR below that. */
   vkGetBufferDeviceAddress =
      (PFN_vkGetBufferDeviceAddress)resolve_device_fn(inst,
                                                      "vkGetBufferDeviceAddress");
   if (!vkGetBufferDeviceAddress)
      vkGetBufferDeviceAddress = (PFN_vkGetBufferDeviceAddress)resolve_device_fn(
         inst, "vkGetBufferDeviceAddressKHR");
   if (!vkGetBufferDeviceAddress)
      note_missing("vkGetBufferDeviceAddress[KHR]");

   if (missing_count) {
      fail("%d entry point(s) missing — the extension is advertised but not "
           "plumbed through the guest ICD / transport",
           missing_count);
      return verdict();
   }
   ok("all core, compute and ray-tracing entry points resolved via "
      "vkGetDeviceProcAddr");

   VkQueue queue;
   vkGetDeviceQueue(dev, qfi, 0, &queue);

   /* ------------------------------------------------------------------ */
   stage("build-sizes");
   /* ------------------------------------------------------------------ */

   /* The one triangle, in the z = 1 plane. */
   static const float verts[3][3] = {
      {0.0f, 0.0f, 1.0f}, {1.0f, 0.0f, 1.0f}, {0.0f, 1.0f, 1.0f}};
   struct buf vb;
   if (!make_buf(&vb, sizeof verts,
                 VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR,
                 1, "vertices"))
      return verdict();
   memcpy(vb.map, verts, sizeof verts);
   buf_flush(&vb);

   VkAccelerationStructureGeometryKHR tri_geom = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
      .geometryType = VK_GEOMETRY_TYPE_TRIANGLES_KHR,
      .flags = VK_GEOMETRY_OPAQUE_BIT_KHR,
      .geometry.triangles = {
         .sType =
            VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_TRIANGLES_DATA_KHR,
         .vertexFormat = VK_FORMAT_R32G32B32_SFLOAT,
         .vertexData.deviceAddress = vb.addr,
         .vertexStride = 12,
         .maxVertex = 2,
         .indexType = VK_INDEX_TYPE_NONE_KHR,
      },
   };
   VkAccelerationStructureBuildGeometryInfoKHR blas_info = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
      .flags = VK_BUILD_ACCELERATION_STRUCTURE_PREFER_FAST_TRACE_BIT_KHR,
      .mode = VK_BUILD_ACCELERATION_STRUCTURE_MODE_BUILD_KHR,
      .geometryCount = 1,
      .pGeometries = &tri_geom,
   };
   uint32_t one = 1;
   VkAccelerationStructureBuildSizesInfoKHR blas_sizes = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_SIZES_INFO_KHR,
   };
   vkGetAccelerationStructureBuildSizesKHR(
      dev, VK_ACCELERATION_STRUCTURE_BUILD_TYPE_DEVICE_KHR, &blas_info, &one,
      &blas_sizes);
   info("BLAS (1 triangle): structure %llu  buildScratch %llu  updateScratch "
        "%llu",
        (unsigned long long)blas_sizes.accelerationStructureSize,
        (unsigned long long)blas_sizes.buildScratchSize,
        (unsigned long long)blas_sizes.updateScratchSize);

   VkAccelerationStructureInstanceKHR inst_data = {
      .transform = {{{1, 0, 0, 0}, {0, 1, 0, 0}, {0, 0, 1, 0}}},
      .instanceCustomIndex = CUSTOM_INDEX,
      .mask = 0xFF,
      .flags = VK_GEOMETRY_INSTANCE_TRIANGLE_FACING_CULL_DISABLE_BIT_KHR,
      /* accelerationStructureReference is filled in once the BLAS exists. */
   };
   struct buf ib;
   if (!make_buf(&ib, sizeof inst_data,
                 VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR,
                 1, "instances"))
      return verdict();

   VkAccelerationStructureGeometryKHR inst_geom = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_KHR,
      .geometryType = VK_GEOMETRY_TYPE_INSTANCES_KHR,
      .geometry.instances = {
         .sType =
            VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_GEOMETRY_INSTANCES_DATA_KHR,
         .arrayOfPointers = VK_FALSE,
         .data.deviceAddress = ib.addr,
      },
   };
   VkAccelerationStructureBuildGeometryInfoKHR tlas_info = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_GEOMETRY_INFO_KHR,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL_KHR,
      .flags = VK_BUILD_ACCELERATION_STRUCTURE_PREFER_FAST_TRACE_BIT_KHR,
      .mode = VK_BUILD_ACCELERATION_STRUCTURE_MODE_BUILD_KHR,
      .geometryCount = 1,
      .pGeometries = &inst_geom,
   };
   VkAccelerationStructureBuildSizesInfoKHR tlas_sizes = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_BUILD_SIZES_INFO_KHR,
   };
   vkGetAccelerationStructureBuildSizesKHR(
      dev, VK_ACCELERATION_STRUCTURE_BUILD_TYPE_DEVICE_KHR, &tlas_info, &one,
      &tlas_sizes);
   info("TLAS (1 instance): structure %llu  buildScratch %llu  updateScratch "
        "%llu",
        (unsigned long long)tlas_sizes.accelerationStructureSize,
        (unsigned long long)tlas_sizes.buildScratchSize,
        (unsigned long long)tlas_sizes.updateScratchSize);

   /* A zero here means the query never reached the host, or reached it and the
    * reply was dropped. Either way nothing downstream can be trusted. */
   if (blas_sizes.accelerationStructureSize == 0 ||
       tlas_sizes.accelerationStructureSize == 0) {
      fail("a returned accelerationStructureSize is 0 — the size query did not "
           "make it through");
      return verdict();
   }
   ok("both size queries returned non-zero sizes");

   /* ------------------------------------------------------------------ */
   stage("as-create");
   /* ------------------------------------------------------------------ */

   struct buf blas_store, blas_scratch, tlas_store, tlas_scratch;
   if (!make_buf(&blas_store, blas_sizes.accelerationStructureSize,
                 VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR, 0,
                 "blas-store"))
      return verdict();
   /* Scratch is addressed at an aligned offset, so over-allocate by the
    * alignment rather than assuming vkGetBufferDeviceAddress hands back an
    * address that already satisfies it. */
   if (!make_buf(&blas_scratch,
                 (blas_sizes.buildScratchSize ? blas_sizes.buildScratchSize
                                              : 256) +
                    scratch_align,
                 VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, 0, "blas-scratch"))
      return verdict();
   if (!make_buf(&tlas_store, tlas_sizes.accelerationStructureSize,
                 VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR, 0,
                 "tlas-store"))
      return verdict();
   if (!make_buf(&tlas_scratch,
                 (tlas_sizes.buildScratchSize ? tlas_sizes.buildScratchSize
                                              : 256) +
                    scratch_align,
                 VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, 0, "tlas-scratch"))
      return verdict();

   VkAccelerationStructureKHR blas;
   VkAccelerationStructureCreateInfoKHR blas_ci = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_CREATE_INFO_KHR,
      .buffer = blas_store.buffer,
      .size = blas_sizes.accelerationStructureSize,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
   };
   r = vkCreateAccelerationStructureKHR(dev, &blas_ci, NULL, &blas);
   if (r != VK_SUCCESS) {
      fail("vkCreateAccelerationStructureKHR(BLAS) -> %d", r);
      return verdict();
   }

   VkAccelerationStructureDeviceAddressInfoKHR adi = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_DEVICE_ADDRESS_INFO_KHR,
      .accelerationStructure = blas,
   };
   VkDeviceAddress blas_handle =
      vkGetAccelerationStructureDeviceAddressKHR(dev, &adi);
   info("BLAS device address 0x%llx", (unsigned long long)blas_handle);
   if (blas_handle == 0) {
      fail("vkGetAccelerationStructureDeviceAddressKHR(BLAS) returned 0 — the "
           "TLAS cannot reference it");
      return verdict();
   }

   /* Now that the BLAS handle exists, finish the instance record. */
   inst_data.accelerationStructureReference = blas_handle;
   memcpy(ib.map, &inst_data, sizeof inst_data);
   buf_flush(&ib);

   VkAccelerationStructureKHR tlas;
   VkAccelerationStructureCreateInfoKHR tlas_ci = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_CREATE_INFO_KHR,
      .buffer = tlas_store.buffer,
      .size = tlas_sizes.accelerationStructureSize,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL_KHR,
   };
   r = vkCreateAccelerationStructureKHR(dev, &tlas_ci, NULL, &tlas);
   if (r != VK_SUCCESS) {
      fail("vkCreateAccelerationStructureKHR(TLAS) -> %d", r);
      return verdict();
   }
   ok("BLAS and TLAS objects created");

   /* ------------------------------------------------------------------ */
   stage("pipeline");
   /* ------------------------------------------------------------------ */

   struct buf out;
   if (!make_buf(&out, NRAYS * sizeof(struct result),
                 VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, 1, "results"))
      return verdict();
   /* Poison the readback buffer: 0xCD everywhere means "the shader never wrote
    * here", which is distinguishable from a real all-zero miss. */
   memset(out.map, 0xCD, NRAYS * sizeof(struct result));
   buf_flush(&out);

   VkDescriptorSetLayoutBinding binds[2] = {
      {
         .binding = 0,
         .descriptorType = VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR,
         .descriptorCount = 1,
         .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
      },
      {
         .binding = 1,
         .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
         .descriptorCount = 1,
         .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
      },
   };
   VkDescriptorSetLayoutCreateInfo dslci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 2,
      .pBindings = binds,
   };
   VkDescriptorSetLayout dsl;
   r = vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl);
   if (r != VK_SUCCESS) {
      fail("vkCreateDescriptorSetLayout with an ACCELERATION_STRUCTURE binding "
           "-> %d",
           r);
      return verdict();
   }
   ok("descriptor set layout with a VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR "
      "binding");

   VkDescriptorPoolSize psizes[2] = {
      {VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR, 1},
      {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1},
   };
   VkDescriptorPoolCreateInfo dpci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 1,
      .poolSizeCount = 2,
      .pPoolSizes = psizes,
   };
   VkDescriptorPool dpool;
   r = vkCreateDescriptorPool(dev, &dpci, NULL, &dpool);
   if (r != VK_SUCCESS) {
      fail("vkCreateDescriptorPool -> %d", r);
      return verdict();
   }

   VkDescriptorSetAllocateInfo dsai = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dpool,
      .descriptorSetCount = 1,
      .pSetLayouts = &dsl,
   };
   VkDescriptorSet dset;
   r = vkAllocateDescriptorSets(dev, &dsai, &dset);
   if (r != VK_SUCCESS) {
      fail("vkAllocateDescriptorSets -> %d", r);
      return verdict();
   }

   VkWriteDescriptorSetAccelerationStructureKHR as_write = {
      .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET_ACCELERATION_STRUCTURE_KHR,
      .accelerationStructureCount = 1,
      .pAccelerationStructures = &tlas,
   };
   VkDescriptorBufferInfo out_info = {
      .buffer = out.buffer,
      .offset = 0,
      .range = VK_WHOLE_SIZE,
   };
   VkWriteDescriptorSet writes[2] = {
      {
         .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
         .pNext = &as_write,
         .dstSet = dset,
         .dstBinding = 0,
         .descriptorCount = 1,
         .descriptorType = VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR,
      },
      {
         .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
         .dstSet = dset,
         .dstBinding = 1,
         .descriptorCount = 1,
         .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
         .pBufferInfo = &out_info,
      },
   };
   vkUpdateDescriptorSets(dev, 2, writes, 0, NULL);
   ok("vkUpdateDescriptorSets wrote the TLAS into the descriptor set");

   /* A plain VkShaderModule, deliberately: the maintenance5 inline-module path
    * puts a VkShaderModuleCreateInfo in a pNext chain, and an unrecognised
    * pNext is exactly the class of thing that makes the guest transport log
    * "Unhandled Vulkan structure type". */
   VkShaderModuleCreateInfo smci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof rq_spv,
      .pCode = rq_spv,
   };
   VkShaderModule mod;
   r = vkCreateShaderModule(dev, &smci, NULL, &mod);
   if (r != VK_SUCCESS) {
      fail("vkCreateShaderModule(%zu bytes of ray-query SPIR-V) -> %d",
           sizeof rq_spv, r);
      return verdict();
   }
   info("shader module created from %zu bytes of SPIR-V", sizeof rq_spv);

   VkPipelineLayoutCreateInfo plci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1,
      .pSetLayouts = &dsl,
   };
   VkPipelineLayout playout;
   r = vkCreatePipelineLayout(dev, &plci, NULL, &playout);
   if (r != VK_SUCCESS) {
      fail("vkCreatePipelineLayout -> %d", r);
      return verdict();
   }

   VkComputePipelineCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_COMPUTE_BIT,
         .module = mod,
         .pName = "main",
      },
      .layout = playout,
   };
   VkPipeline pipe = VK_NULL_HANDLE;
   r = vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe);
   if (r != VK_SUCCESS) {
      fail("vkCreateComputePipelines from SPIR-V with OpRayQueryInitializeKHR "
           "-> %d (the SPIR-V RayQueryKHR capability did not survive the "
           "guest ICD, the transport, or the NIR->MSL translation)",
           r);
      return verdict();
   }
   ok("compute pipeline built from SPIR-V using the RayQueryKHR capability");

   /* ------------------------------------------------------------------ */
   stage("as-build");
   /* ------------------------------------------------------------------ */

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qfi,
   };
   VkCommandPool pool;
   r = vkCreateCommandPool(dev, &pci, NULL, &pool);
   if (r != VK_SUCCESS) {
      fail("vkCreateCommandPool -> %d", r);
      return verdict();
   }

   VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   r = vkAllocateCommandBuffers(dev, &cbai, &cmd);
   if (r != VK_SUCCESS) {
      fail("vkAllocateCommandBuffers -> %d", r);
      return verdict();
   }

   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   r = vkBeginCommandBuffer(cmd, &bi);
   if (r != VK_SUCCESS) {
      fail("vkBeginCommandBuffer -> %d", r);
      return verdict();
   }

   blas_info.dstAccelerationStructure = blas;
   blas_info.scratchData.deviceAddress =
      align_up(blas_scratch.addr, scratch_align);
   VkAccelerationStructureBuildRangeInfoKHR blas_range = {.primitiveCount = 1};
   const VkAccelerationStructureBuildRangeInfoKHR *blas_ranges = &blas_range;
   vkCmdBuildAccelerationStructuresKHR(cmd, 1, &blas_info, &blas_ranges);

   VkMemoryBarrier mb = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_WRITE_BIT_KHR,
      .dstAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_READ_BIT_KHR,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR,
                        VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR, 0,
                        1, &mb, 0, NULL, 0, NULL);

   tlas_info.dstAccelerationStructure = tlas;
   tlas_info.scratchData.deviceAddress =
      align_up(tlas_scratch.addr, scratch_align);
   VkAccelerationStructureBuildRangeInfoKHR tlas_range = {.primitiveCount = 1};
   const VkAccelerationStructureBuildRangeInfoKHR *tlas_ranges = &tlas_range;
   vkCmdBuildAccelerationStructuresKHR(cmd, 1, &tlas_info, &tlas_ranges);

   VkMemoryBarrier mb2 = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_WRITE_BIT_KHR,
      .dstAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_READ_BIT_KHR |
                       VK_ACCESS_SHADER_READ_BIT,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR,
                        VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 1, &mb2, 0, NULL,
                        0, NULL);

   /* ------------------------------------------------------------------ */
   /* The dispatch rides in the same command buffer as the build, so that a     */
   /* single fence proves both. Stage naming stays honest by checking the       */
   /* fence under "as-build" and the contents under "ray-hit"/"ray-miss".       */
   /* ------------------------------------------------------------------ */

   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, playout, 0, 1,
                           &dset, 0, NULL);
   vkCmdDispatch(cmd, 1, 1, 1);

   VkMemoryBarrier mb3 = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                        VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb3, 0, NULL, 0, NULL);

   r = vkEndCommandBuffer(cmd);
   if (r != VK_SUCCESS) {
      fail("vkEndCommandBuffer -> %d", r);
      return verdict();
   }

   VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   VkFence fence;
   r = vkCreateFence(dev, &fci, NULL, &fence);
   if (r != VK_SUCCESS) {
      fail("vkCreateFence -> %d", r);
      return verdict();
   }
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   r = vkQueueSubmit(queue, 1, &si, fence);
   if (r != VK_SUCCESS) {
      fail("vkQueueSubmit -> %d", r);
      return verdict();
   }
   info("submitted: BLAS build, TLAS build, ray-query dispatch");

   /* 30s: generous for a cold guest going all the way to Metal, short enough
    * that a lost fence is a failure rather than a hang. */
   r = vkWaitForFences(dev, 1, &fence, VK_TRUE, 30ull * 1000000000ull);
   if (r != VK_SUCCESS) {
      fail("vkWaitForFences -> %d%s", r,
           r == VK_TIMEOUT ? " (VK_TIMEOUT — the fence never signalled; the "
                             "submission is stuck below the guest)"
                           : "");
      return verdict();
   }
   ok("acceleration structure builds and the ray-query dispatch completed, "
      "fence signalled");

   buf_invalidate(&out);
   const struct result *res = out.map;

   printf("\n");
   for (int i = 0; i < NRAYS; i++) {
      printf("  ray %d: type=%u t=%.4f prim=%u geom=%u inst=%u custom=0x%x "
             "bary=(%.4f,%.4f) ff=%u cands=%u tmin=%.4f flags=0x%x\n",
             i, res[i].committed_type, res[i].t, res[i].prim, res[i].geom,
             res[i].instance_id, res[i].custom_index, res[i].bary_x,
             res[i].bary_y, res[i].front_face, res[i].candidates_seen,
             res[i].ray_tmin, res[i].ray_flags);
   }

   /* 0xCDCDCDCD anywhere means the dispatch never ran, which is a different
    * bug from a wrong intersection and must not be read as one. */
   if (res[0].committed_type == 0xCDCDCDCDu) {
      stage("dispatch");
      fail("the results buffer is still poison (0xCDCDCDCD) — the fence "
           "signalled but the compute shader never wrote anything");
      return verdict();
   }

   /* ------------------------------------------------------------------ */
   stage("ray-hit");
   /* ------------------------------------------------------------------ */

   int hit_ok = 1;
   if (res[0].committed_type != 1) {
      fail("ray 0 committed type = %u, want 1 (triangle). Every ray missing "
           "while the miss path passes is the signature of an acceleration "
           "structure that built cleanly and contains nothing — on Apple8 that "
           "is the Metal 4 software-BVH build limitation, not this probe.",
           res[0].committed_type);
      hit_ok = 0;
   } else {
      ok("ray 0 committed a TRIANGLE intersection");
   }
   if (hit_ok) {
      if (!near_eq(res[0].t, 1.0f)) {
         fail("ray 0 t = %.6f, want 1.0", res[0].t);
         hit_ok = 0;
      } else
         ok("ray 0 hit distance t = %.6f", res[0].t);

      if (res[0].prim != 0 || res[0].geom != 0 || res[0].instance_id != 0) {
         fail("ray 0 prim/geom/instance = %u/%u/%u, want 0/0/0", res[0].prim,
              res[0].geom, res[0].instance_id);
         hit_ok = 0;
      } else
         ok("ray 0 primitive/geometry/instance index = 0/0/0");

      if (res[0].custom_index != CUSTOM_INDEX) {
         fail("ray 0 instanceCustomIndex = 0x%x, want 0x%x",
              res[0].custom_index, CUSTOM_INDEX);
         hit_ok = 0;
      } else
         ok("ray 0 instanceCustomIndex = 0x%x (the TLAS instance record "
            "survived the round trip)",
            res[0].custom_index);

      /* hit point (0.25, 0.25, 1) on the triangle v0 (0,0,1) v1 (1,0,1)
       * v2 (0,1,1), so under Vulkan's convention
       * hit = v0*(1-b1-b2) + v1*b1 + v2*b2 the barycentrics are exactly
       * (0.25, 0.25). Note the honest limitation: those two are EQUAL, so this
       * check cannot detect a driver that swaps b1 and b2. The values are
       * printed above regardless, which is the evidence of record; the ray
       * geometry is kept identical to the host-side rq-smoke.c so the two runs
       * are directly comparable, and that comparability is worth more here than
       * detecting a swap that no observed failure mode produces. */
      if (!near_eq(res[0].bary_x, 0.25f) || !near_eq(res[0].bary_y, 0.25f)) {
         fail("ray 0 barycentrics = (%.6f, %.6f), want (0.25, 0.25)",
              res[0].bary_x, res[0].bary_y);
         hit_ok = 0;
      } else
         ok("ray 0 barycentrics = (%.6f, %.6f) — analytically exact for this "
            "triangle",
            res[0].bary_x, res[0].bary_y);
   }

   /* The ray-state accessors are answered by the shader, not by the BVH, so
    * they are checked separately: they can pass while intersection fails, and
    * that distinction is the whole diagnostic value of this probe. */
   if (!near_eq(res[0].ray_tmin, 0.001f))
      fail("rayQueryGetRayTMinEXT = %.6f, want 0.001", res[0].ray_tmin);
   else
      ok("rayQueryGetRayTMinEXT = %.6f", res[0].ray_tmin);
   if (res[0].ray_flags != 1u)
      fail("rayQueryGetRayFlagsEXT = 0x%x, want 0x1 (gl_RayFlagsOpaque)",
           res[0].ray_flags);
   else
      ok("rayQueryGetRayFlagsEXT = 0x%x (gl_RayFlagsOpaque)", res[0].ray_flags);

   /* ------------------------------------------------------------------ */
   stage("ray-miss");
   /* ------------------------------------------------------------------ */

   int misses = 0;
   for (int i = 1; i < NRAYS; i++)
      misses += (res[i].committed_type == 0);
   if (misses != NRAYS - 1)
      fail("rays 1..%d: only %d of %d missed — a driver that hits everything is "
           "as broken as one that hits nothing",
           NRAYS - 1, misses, NRAYS - 1);
   else
      ok("rays 1..%d all missed (%d of %d)", NRAYS - 1, misses, NRAYS - 1);

   /* ------------------------------------------------------------------ */
   stage("teardown");
   /* ------------------------------------------------------------------ */

   vkDeviceWaitIdle(dev);
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyPipeline(dev, pipe, NULL);
   vkDestroyShaderModule(dev, mod, NULL);
   vkDestroyPipelineLayout(dev, playout, NULL);
   vkDestroyDescriptorPool(dev, dpool, NULL);
   vkDestroyDescriptorSetLayout(dev, dsl, NULL);
   vkDestroyAccelerationStructureKHR(dev, tlas, NULL);
   vkDestroyAccelerationStructureKHR(dev, blas, NULL);
   ok("destroyed cleanly (a teardown crash here would be its own finding)");

   /* Interpretation, so the Run phase does not have to reconstruct it. */
   printf("\nreading:\n");
   if (!failed) {
      printf("  Ray tracing works end to end in the guest: a real hit with the\n"
             "  analytically correct distance and barycentrics, and a real miss.\n");
   } else if (res[0].committed_type == 0 && misses == NRAYS - 1) {
      printf("  Everything above the BVH works — extensions, features, device\n"
             "  creation, entry points, ACCELERATION_STRUCTURE descriptors,\n"
             "  SPIR-V RayQueryKHR through NIR to MSL, pipeline creation,\n"
             "  dispatch, the ray-state accessors and the miss path. Nothing\n"
             "  intersects: the structure built cleanly and contains nothing.\n"
             "\n"
             "  This is the GUEST-ONLY failure, not the old Metal 4 builder one.\n"
             "  The host-side twin, kosmickrisp/run-rq-smoke.sh, scores 16/16\n"
             "  with a real hit on this same GPU, so the builder and the emulated\n"
             "  BVH are fine. Run the host renderer with MESA_KK_RT_DEBUG=1: it\n"
             "  will show the legacy builder selected, the legacy queue created\n"
             "  for this VkDevice, BLAS and TLAS built with 'descriptor ok', and\n"
             "  no unresolvable device address — i.e. the bug is in what the\n"
             "  transport hands the build, or in how the split submission is\n"
             "  ordered when the host replays this command buffer.\n");
   } else {
      printf("  Wrong intersection results, with at least one ray behaving.\n"
             "  This is not the Metal 4 build limitation — that misses\n"
             "  uniformly. Look for a real driver or transport bug.\n");
   }

   return verdict();
}
