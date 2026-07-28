// kk-gate.c — Phase 2 gate for the MoltenVK -> KosmicKrisp migration, plus the Phase-0.6 identity dump.
//
// Two modes, one binary (one binary because both need the exact same loader/ICD environment, and a
// second executable would be a second thing to keep in sync with run-gate.sh):
//
//   kk-gate                    the capability GATE (default; unchanged, load-bearing)
//   kk-gate --dump-identity    machine-diffable JSON of what this host driver reports, on stdout
//
// GATE. vulkaninfo only proves the loader can enumerate the driver. This proves the three things
// gfxstream actually stands on, and nothing else:
//   1. Device creation with VK_EXT_external_memory_metal enabled.
//   2. An external-memory VkImage exported as an MTLHeap — gfxstream's ColorBuffer path uses exactly
//      VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT, and KosmicKrisp supports *only* that handle type.
//   3. Real queue submission reaching the GPU (clear + fence wait), i.e. Metal encoding works.
//
// --dump-identity. Nothing in this repo recorded what the host driver actually reports, so every
// GPU-identity change was unverifiable: there was no before to diff the after against. This emits the
// whole first-order identity surface — VkPhysicalDeviceProperties + its pNext chain, every
// VkPhysicalDeviceFeatures bool by name, the sorted device-extension list, the BC1..BC7 format
// properties, and the first 32 bytes of vkGetPipelineCacheData on a fresh empty cache.
//
// That last one is the point. vkGetPipelineCacheData is a raw passthrough in gfxstream (there is no
// on_vkGetPipelineCacheData in VkDecoderGlobalState; VkDecoder.cpp:4403 calls the driver directly),
// and Mesa builds the 32-byte VkPipelineCacheHeaderVersionOne out of the *real* vendorID (bytes
// 8..12), deviceID (12..16) and pipelineCacheUUID (16..32). So any app can recover all three from a
// pipeline cache blob no matter what the properties call said. The dump decodes and labels those
// three fields next to the raw hex so the leak is self-evident, and flags whether each one agrees
// with the properties call — after a spoof lands, the agreement flags are the regression test.
//
// The output is a golden file: stable key order, extensions sorted by name, no timestamps, no
// pointers, no paths, floats at %.9g (exact float32 round-trip, locale-independent in the C locale).
// A rerun on an unchanged host must be byte-identical. JSON is hand-emitted; there is deliberately no
// JSON dependency, because this must build with nothing but the Vulkan headers.
//
// Build: clang -o kk-gate kk-gate.c -I$(brew --prefix vulkan-headers)/include -L/opt/homebrew/lib -lvulkan
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define VK_ENABLE_BETA_EXTENSIONS
#define VK_USE_PLATFORM_METAL_EXT
#include <vulkan/vulkan.h>

#define CHECK(expr)                                                            \
    do {                                                                       \
        VkResult _r = (expr);                                                  \
        if (_r != VK_SUCCESS) {                                                \
            printf("FAIL %s -> VkResult %d\n", #expr, (int)_r);                \
            return 1;                                                          \
        }                                                                      \
    } while (0)

static int run_gate(void) {
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "kk-gate",
                             .apiVersion = VK_API_VERSION_1_3};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance inst;
    CHECK(vkCreateInstance(&ici, NULL, &inst));

    uint32_t n = 0;
    CHECK(vkEnumeratePhysicalDevices(inst, &n, NULL));
    if (!n) { printf("FAIL no physical devices\n"); return 1; }
    VkPhysicalDevice *pds = calloc(n, sizeof *pds);
    CHECK(vkEnumeratePhysicalDevices(inst, &n, pds));
    VkPhysicalDevice pd = pds[0];

    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    printf("device      : %s (api %u.%u.%u)\n", props.deviceName, VK_VERSION_MAJOR(props.apiVersion),
           VK_VERSION_MINOR(props.apiVersion), VK_VERSION_PATCH(props.apiVersion));

    VkPhysicalDeviceDriverProperties drv = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES};
    VkPhysicalDeviceProperties2 p2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &drv};
    vkGetPhysicalDeviceProperties2(pd, &p2);
    printf("driver      : %s / %s (driverID %d)\n", drv.driverName, drv.driverInfo, (int)drv.driverID);

    // ---- 1. queue family + device with VK_EXT_external_memory_metal ----
    uint32_t qn = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, NULL);
    VkQueueFamilyProperties *qs = calloc(qn, sizeof *qs);
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, qs);
    uint32_t qfi = UINT32_MAX;
    for (uint32_t i = 0; i < qn; i++)
        if (qs[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) { qfi = i; break; }
    if (qfi == UINT32_MAX) { printf("FAIL no graphics queue family\n"); return 1; }

    const char *devExts[] = {"VK_EXT_external_memory_metal", "VK_KHR_external_memory",
                             "VK_KHR_dedicated_allocation", "VK_KHR_get_memory_requirements2"};
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueFamilyIndex = qfi, .queueCount = 1, .pQueuePriorities = &prio};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                              .enabledExtensionCount = 4, .ppEnabledExtensionNames = devExts};
    VkDevice dev;
    CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
    printf("PASS 1/3    : vkCreateDevice with VK_EXT_external_memory_metal\n");

    VkQueue q;
    vkGetDeviceQueue(dev, qfi, 0, &q);

    // ---- 2. external-memory image exported as an MTLHeap (the gfxstream ColorBuffer shape) ----
    VkExternalMemoryImageCreateInfo emici = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT};
    VkImageCreateInfo imgci = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                               .pNext = &emici,
                               .imageType = VK_IMAGE_TYPE_2D,
                               .format = VK_FORMAT_R8G8B8A8_UNORM,
                               .extent = {256, 256, 1},
                               .mipLevels = 1, .arrayLayers = 1,
                               .samples = VK_SAMPLE_COUNT_1_BIT,
                               .tiling = VK_IMAGE_TILING_OPTIMAL,
                               .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT |
                                        VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
                               .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED};
    VkImage img;
    CHECK(vkCreateImage(dev, &imgci, NULL, &img));

    VkMemoryRequirements mr;
    vkGetImageMemoryRequirements(dev, img, &mr);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t mt = UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((mr.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) { mt = i; break; }
    if (mt == UINT32_MAX) { printf("FAIL no device-local memory type\n"); return 1; }

    VkExportMemoryAllocateInfo emai = {.sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                                       .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT};
    VkMemoryDedicatedAllocateInfo mdai = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                                          .pNext = &emai, .image = img};
    VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                .pNext = &mdai, .allocationSize = mr.size, .memoryTypeIndex = mt};
    VkDeviceMemory mem;
    CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
    CHECK(vkBindImageMemory(dev, img, mem, 0));

    PFN_vkGetMemoryMetalHandleEXT getHandle =
        (PFN_vkGetMemoryMetalHandleEXT)vkGetDeviceProcAddr(dev, "vkGetMemoryMetalHandleEXT");
    if (!getHandle) { printf("FAIL vkGetMemoryMetalHandleEXT not resolvable\n"); return 1; }
    VkMemoryGetMetalHandleInfoEXT ghi = {.sType = VK_STRUCTURE_TYPE_MEMORY_GET_METAL_HANDLE_INFO_EXT,
                                         .memory = mem,
                                         .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT};
    void *mtlHeap = NULL;
    CHECK(getHandle(dev, &ghi, &mtlHeap));
    if (!mtlHeap) { printf("FAIL vkGetMemoryMetalHandleEXT returned VK_SUCCESS but a NULL MTLHeap\n"); return 1; }
    printf("PASS 2/3    : MTLHeap exported from an external-memory image -> %p\n", mtlHeap);

    // ---- 3. real submission: clear the image and wait on a fence ----
    VkCommandPoolCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qfi};
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
    VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                        .commandPool = pool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    VkCommandBuffer cb;
    CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                   .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    CHECK(vkBeginCommandBuffer(cb, &bi));
    VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    VkImageMemoryBarrier toDst = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                                  .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                  .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                  .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                  .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                  .image = img, .subresourceRange = range,
                                  .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT};
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL,
                         0, NULL, 1, &toDst);
    VkClearColorValue color = {.float32 = {0.1f, 0.7f, 0.3f, 1.0f}};
    vkCmdClearColorImage(cb, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &color, 1, &range);
    CHECK(vkEndCommandBuffer(cb));

    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    CHECK(vkCreateFence(dev, &fci, NULL, &fence));
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
    CHECK(vkQueueSubmit(q, 1, &si, fence));
    VkResult wr = vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000);
    if (wr != VK_SUCCESS) { printf("FAIL fence wait -> %d (GPU never completed the clear)\n", (int)wr); return 1; }
    printf("PASS 3/3    : queue submit + clear + fence signalled (GPU executed real work)\n");

    vkDestroyFence(dev, fence, NULL);
    vkDestroyCommandPool(dev, pool, NULL);
    vkDestroyImage(dev, img, NULL);
    vkFreeMemory(dev, mem, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    printf("\nGATE PASSED\n");
    return 0;
}

// ===========================================================================
// --dump-identity
// ===========================================================================
//
// Everything below writes JSON to stdout and diagnostics to stderr — never the other way round, or a
// single warning would corrupt the golden file. CHECK above prints to stdout, which is correct for the
// gate and wrong here, hence the separate DCHECK/DFAIL.

#define DFAIL(...)                                                             \
    do {                                                                       \
        fprintf(stderr, __VA_ARGS__);                                          \
        return 1;                                                              \
    } while (0)

#define DCHECK(expr)                                                           \
    do {                                                                       \
        VkResult _r = (expr);                                                  \
        if (_r != VK_SUCCESS) {                                                \
            fprintf(stderr, "FAIL %s -> VkResult %d\n", #expr, (int)_r);       \
            return 1;                                                          \
        }                                                                      \
    } while (0)

// ---- minimal JSON emitter -------------------------------------------------
// Containers track their own "have I written a member yet" flag, so a comma is emitted *before* each
// member rather than after — a trailing comma is invalid JSON and the cheapest way to never emit one
// is to never be in a position where you might.

static int jdepth = 0;

static void jind(void) {
    for (int i = 0; i < jdepth; i++) fputs("  ", stdout);
}

// Start a member. `k == NULL` for array elements, which have no key.
static void jkey(int *first, const char *k) {
    if (!*first) putchar(',');
    *first = 0;
    putchar('\n');
    jind();
    if (k) printf("\"%s\": ", k);
}

static void jopen(int *first, const char *k, char brace) {
    jkey(first, k);
    putchar(brace);
    jdepth++;
}

// `first` is the container's own flag: still set means the container stayed empty, so emit `{}`/`[]`
// on one line rather than a brace pair around a bare newline.
static void jclose(int first, char brace) {
    jdepth--;
    if (!first) {
        putchar('\n');
        jind();
    }
    putchar(brace);
}

static void jputstr(const char *s) {
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
        if (*p == '"' || *p == '\\') printf("\\%c", *p);
        else if (*p < 0x20 || *p == 0x7f) printf("\\u%04x", *p);
        else putchar(*p);
    }
    putchar('"');
}

static void jstr(int *f, const char *k, const char *v) { jkey(f, k); jputstr(v); }
static void jbool(int *f, const char *k, VkBool32 v) { jkey(f, k); fputs(v ? "true" : "false", stdout); }
static void jnull(int *f, const char *k) { jkey(f, k); fputs("null", stdout); }
static void ju32(int *f, const char *k, uint32_t v) { jkey(f, k); printf("%u", v); }
static void ji32(int *f, const char *k, int32_t v) { jkey(f, k); printf("%d", v); }
static void ju64(int *f, const char *k, uint64_t v) { jkey(f, k); printf("%llu", (unsigned long long)v); }

// Bitmask fields go out as a hex STRING, not a number: a mask is not an arithmetic quantity, and
// "0x0000001f" diffs far more legibly than 31.
static void jflags(int *f, const char *k, uint64_t v) { jkey(f, k); printf("\"0x%08llx\"", (unsigned long long)v); }
static void jhex32(int *f, const char *k, uint32_t v) { jkey(f, k); printf("\"0x%08x\"", v); }

// %.9g round-trips every float32 exactly and, in the C locale (we never call setlocale), always emits
// '.' as the decimal point. Infinities and NaN have no JSON spelling, so they become strings.
static void jf32(int *f, const char *k, float v) {
    jkey(f, k);
    if (isnan(v)) fputs("\"nan\"", stdout);
    else if (isinf(v)) fputs(v > 0 ? "\"inf\"" : "\"-inf\"", stdout);
    else printf("%.9g", (double)v);
}

static void ju32arr(int *f, const char *k, const uint32_t *v, int n) {
    int a = 1;
    jopen(f, k, '[');
    for (int i = 0; i < n; i++) ju32(&a, NULL, v[i]);
    jclose(a, ']');
}

static void jf32arr(int *f, const char *k, const float *v, int n) {
    int a = 1;
    jopen(f, k, '[');
    for (int i = 0; i < n; i++) jf32(&a, NULL, v[i]);
    jclose(a, ']');
}

// Vulkan's own convention. driverVersion is vendor-encoded and this decode is only *conventional* for
// it, which is why the raw value is emitted beside every text form.
static void jver(int *f, const char *k, uint32_t v) {
    jkey(f, k);
    printf("\"%u.%u.%u\"", VK_API_VERSION_MAJOR(v), VK_API_VERSION_MINOR(v), VK_API_VERSION_PATCH(v));
}

static void jhexbytes(int *f, const char *k, const uint8_t *b, size_t n) {
    jkey(f, k);
    putchar('"');
    for (size_t i = 0; i < n; i++) printf("%02x", b[i]);
    putchar('"');
}

// ---- enum -> name --------------------------------------------------------
// Spelled out rather than derived: the name is the durable half of the record. A dump that says
// `deviceType: 1` and nothing else stops being readable the moment the header moves.

static const char *device_type_name(VkPhysicalDeviceType t) {
    switch (t) {
    case VK_PHYSICAL_DEVICE_TYPE_OTHER: return "VK_PHYSICAL_DEVICE_TYPE_OTHER";
    case VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU: return "VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU";
    case VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU: return "VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU";
    case VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU: return "VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU";
    case VK_PHYSICAL_DEVICE_TYPE_CPU: return "VK_PHYSICAL_DEVICE_TYPE_CPU";
    default: return "UNKNOWN";
    }
}

static const char *driver_id_name(VkDriverId d) {
    switch (d) {
    case VK_DRIVER_ID_AMD_PROPRIETARY: return "VK_DRIVER_ID_AMD_PROPRIETARY";
    case VK_DRIVER_ID_AMD_OPEN_SOURCE: return "VK_DRIVER_ID_AMD_OPEN_SOURCE";
    case VK_DRIVER_ID_MESA_RADV: return "VK_DRIVER_ID_MESA_RADV";
    case VK_DRIVER_ID_NVIDIA_PROPRIETARY: return "VK_DRIVER_ID_NVIDIA_PROPRIETARY";
    case VK_DRIVER_ID_INTEL_PROPRIETARY_WINDOWS: return "VK_DRIVER_ID_INTEL_PROPRIETARY_WINDOWS";
    case VK_DRIVER_ID_INTEL_OPEN_SOURCE_MESA: return "VK_DRIVER_ID_INTEL_OPEN_SOURCE_MESA";
    case VK_DRIVER_ID_IMAGINATION_PROPRIETARY: return "VK_DRIVER_ID_IMAGINATION_PROPRIETARY";
    case VK_DRIVER_ID_QUALCOMM_PROPRIETARY: return "VK_DRIVER_ID_QUALCOMM_PROPRIETARY";
    case VK_DRIVER_ID_ARM_PROPRIETARY: return "VK_DRIVER_ID_ARM_PROPRIETARY";
    case VK_DRIVER_ID_GOOGLE_SWIFTSHADER: return "VK_DRIVER_ID_GOOGLE_SWIFTSHADER";
    case VK_DRIVER_ID_GGP_PROPRIETARY: return "VK_DRIVER_ID_GGP_PROPRIETARY";
    case VK_DRIVER_ID_BROADCOM_PROPRIETARY: return "VK_DRIVER_ID_BROADCOM_PROPRIETARY";
    case VK_DRIVER_ID_MESA_LLVMPIPE: return "VK_DRIVER_ID_MESA_LLVMPIPE";
    case VK_DRIVER_ID_MOLTENVK: return "VK_DRIVER_ID_MOLTENVK";
    case VK_DRIVER_ID_COREAVI_PROPRIETARY: return "VK_DRIVER_ID_COREAVI_PROPRIETARY";
    case VK_DRIVER_ID_JUICE_PROPRIETARY: return "VK_DRIVER_ID_JUICE_PROPRIETARY";
    case VK_DRIVER_ID_VERISILICON_PROPRIETARY: return "VK_DRIVER_ID_VERISILICON_PROPRIETARY";
    case VK_DRIVER_ID_MESA_TURNIP: return "VK_DRIVER_ID_MESA_TURNIP";
    case VK_DRIVER_ID_MESA_V3DV: return "VK_DRIVER_ID_MESA_V3DV";
    case VK_DRIVER_ID_MESA_PANVK: return "VK_DRIVER_ID_MESA_PANVK";
    case VK_DRIVER_ID_SAMSUNG_PROPRIETARY: return "VK_DRIVER_ID_SAMSUNG_PROPRIETARY";
    case VK_DRIVER_ID_MESA_VENUS: return "VK_DRIVER_ID_MESA_VENUS";
    case VK_DRIVER_ID_MESA_DOZEN: return "VK_DRIVER_ID_MESA_DOZEN";
    case VK_DRIVER_ID_MESA_NVK: return "VK_DRIVER_ID_MESA_NVK";
    case VK_DRIVER_ID_IMAGINATION_OPEN_SOURCE_MESA: return "VK_DRIVER_ID_IMAGINATION_OPEN_SOURCE_MESA";
    case VK_DRIVER_ID_MESA_HONEYKRISP: return "VK_DRIVER_ID_MESA_HONEYKRISP";
    case VK_DRIVER_ID_VULKAN_SC_EMULATION_ON_VULKAN: return "VK_DRIVER_ID_VULKAN_SC_EMULATION_ON_VULKAN";
    case VK_DRIVER_ID_MESA_KOSMICKRISP: return "VK_DRIVER_ID_MESA_KOSMICKRISP";
    default: return "UNKNOWN";
    }
}

// ---- the block-compressed formats Phase 4.8 has to zero ------------------
// textureCompressionBC = FALSE is only half the clamp; a device that denies the feature while still
// advertising sampled-image support on BC7 is *more* anomalous than one that admits it. These are the
// formats that have to move together, so they are the formats the golden has to cover.
static const struct {
    VkFormat fmt;
    const char *name;
} kBcFormats[] = {
    {VK_FORMAT_BC1_RGB_UNORM_BLOCK, "VK_FORMAT_BC1_RGB_UNORM_BLOCK"},
    {VK_FORMAT_BC1_RGB_SRGB_BLOCK, "VK_FORMAT_BC1_RGB_SRGB_BLOCK"},
    {VK_FORMAT_BC1_RGBA_UNORM_BLOCK, "VK_FORMAT_BC1_RGBA_UNORM_BLOCK"},
    {VK_FORMAT_BC1_RGBA_SRGB_BLOCK, "VK_FORMAT_BC1_RGBA_SRGB_BLOCK"},
    {VK_FORMAT_BC2_UNORM_BLOCK, "VK_FORMAT_BC2_UNORM_BLOCK"},
    {VK_FORMAT_BC2_SRGB_BLOCK, "VK_FORMAT_BC2_SRGB_BLOCK"},
    {VK_FORMAT_BC3_UNORM_BLOCK, "VK_FORMAT_BC3_UNORM_BLOCK"},
    {VK_FORMAT_BC3_SRGB_BLOCK, "VK_FORMAT_BC3_SRGB_BLOCK"},
    {VK_FORMAT_BC4_UNORM_BLOCK, "VK_FORMAT_BC4_UNORM_BLOCK"},
    {VK_FORMAT_BC4_SNORM_BLOCK, "VK_FORMAT_BC4_SNORM_BLOCK"},
    {VK_FORMAT_BC5_UNORM_BLOCK, "VK_FORMAT_BC5_UNORM_BLOCK"},
    {VK_FORMAT_BC5_SNORM_BLOCK, "VK_FORMAT_BC5_SNORM_BLOCK"},
    {VK_FORMAT_BC6H_UFLOAT_BLOCK, "VK_FORMAT_BC6H_UFLOAT_BLOCK"},
    {VK_FORMAT_BC6H_SFLOAT_BLOCK, "VK_FORMAT_BC6H_SFLOAT_BLOCK"},
    {VK_FORMAT_BC7_UNORM_BLOCK, "VK_FORMAT_BC7_UNORM_BLOCK"},
    {VK_FORMAT_BC7_SRGB_BLOCK, "VK_FORMAT_BC7_SRGB_BLOCK"},
};

static int extcmp(const void *a, const void *b) {
    return strcmp(((const VkExtensionProperties *)a)->extensionName,
                  ((const VkExtensionProperties *)b)->extensionName);
}

static uint32_t rd_le32(const uint8_t *p) {
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

static void emit_limits(int *f, const VkPhysicalDeviceLimits *l) {
    int o = 1;
    jopen(f, "limits", '{');
    ju32(&o, "maxImageDimension1D", l->maxImageDimension1D);
    ju32(&o, "maxImageDimension2D", l->maxImageDimension2D);
    ju32(&o, "maxImageDimension3D", l->maxImageDimension3D);
    ju32(&o, "maxImageDimensionCube", l->maxImageDimensionCube);
    ju32(&o, "maxImageArrayLayers", l->maxImageArrayLayers);
    ju32(&o, "maxTexelBufferElements", l->maxTexelBufferElements);
    ju32(&o, "maxUniformBufferRange", l->maxUniformBufferRange);
    ju32(&o, "maxStorageBufferRange", l->maxStorageBufferRange);
    ju32(&o, "maxPushConstantsSize", l->maxPushConstantsSize);
    ju32(&o, "maxMemoryAllocationCount", l->maxMemoryAllocationCount);
    ju32(&o, "maxSamplerAllocationCount", l->maxSamplerAllocationCount);
    ju64(&o, "bufferImageGranularity", l->bufferImageGranularity);
    ju64(&o, "sparseAddressSpaceSize", l->sparseAddressSpaceSize);
    ju32(&o, "maxBoundDescriptorSets", l->maxBoundDescriptorSets);
    ju32(&o, "maxPerStageDescriptorSamplers", l->maxPerStageDescriptorSamplers);
    ju32(&o, "maxPerStageDescriptorUniformBuffers", l->maxPerStageDescriptorUniformBuffers);
    ju32(&o, "maxPerStageDescriptorStorageBuffers", l->maxPerStageDescriptorStorageBuffers);
    ju32(&o, "maxPerStageDescriptorSampledImages", l->maxPerStageDescriptorSampledImages);
    ju32(&o, "maxPerStageDescriptorStorageImages", l->maxPerStageDescriptorStorageImages);
    ju32(&o, "maxPerStageDescriptorInputAttachments", l->maxPerStageDescriptorInputAttachments);
    ju32(&o, "maxPerStageResources", l->maxPerStageResources);
    ju32(&o, "maxDescriptorSetSamplers", l->maxDescriptorSetSamplers);
    ju32(&o, "maxDescriptorSetUniformBuffers", l->maxDescriptorSetUniformBuffers);
    ju32(&o, "maxDescriptorSetUniformBuffersDynamic", l->maxDescriptorSetUniformBuffersDynamic);
    ju32(&o, "maxDescriptorSetStorageBuffers", l->maxDescriptorSetStorageBuffers);
    ju32(&o, "maxDescriptorSetStorageBuffersDynamic", l->maxDescriptorSetStorageBuffersDynamic);
    ju32(&o, "maxDescriptorSetSampledImages", l->maxDescriptorSetSampledImages);
    ju32(&o, "maxDescriptorSetStorageImages", l->maxDescriptorSetStorageImages);
    ju32(&o, "maxDescriptorSetInputAttachments", l->maxDescriptorSetInputAttachments);
    ju32(&o, "maxVertexInputAttributes", l->maxVertexInputAttributes);
    ju32(&o, "maxVertexInputBindings", l->maxVertexInputBindings);
    ju32(&o, "maxVertexInputAttributeOffset", l->maxVertexInputAttributeOffset);
    ju32(&o, "maxVertexInputBindingStride", l->maxVertexInputBindingStride);
    ju32(&o, "maxVertexOutputComponents", l->maxVertexOutputComponents);
    ju32(&o, "maxTessellationGenerationLevel", l->maxTessellationGenerationLevel);
    ju32(&o, "maxTessellationPatchSize", l->maxTessellationPatchSize);
    ju32(&o, "maxTessellationControlPerVertexInputComponents", l->maxTessellationControlPerVertexInputComponents);
    ju32(&o, "maxTessellationControlPerVertexOutputComponents", l->maxTessellationControlPerVertexOutputComponents);
    ju32(&o, "maxTessellationControlPerPatchOutputComponents", l->maxTessellationControlPerPatchOutputComponents);
    ju32(&o, "maxTessellationControlTotalOutputComponents", l->maxTessellationControlTotalOutputComponents);
    ju32(&o, "maxTessellationEvaluationInputComponents", l->maxTessellationEvaluationInputComponents);
    ju32(&o, "maxTessellationEvaluationOutputComponents", l->maxTessellationEvaluationOutputComponents);
    ju32(&o, "maxGeometryShaderInvocations", l->maxGeometryShaderInvocations);
    ju32(&o, "maxGeometryInputComponents", l->maxGeometryInputComponents);
    ju32(&o, "maxGeometryOutputComponents", l->maxGeometryOutputComponents);
    ju32(&o, "maxGeometryOutputVertices", l->maxGeometryOutputVertices);
    ju32(&o, "maxGeometryTotalOutputComponents", l->maxGeometryTotalOutputComponents);
    ju32(&o, "maxFragmentInputComponents", l->maxFragmentInputComponents);
    ju32(&o, "maxFragmentOutputAttachments", l->maxFragmentOutputAttachments);
    ju32(&o, "maxFragmentDualSrcAttachments", l->maxFragmentDualSrcAttachments);
    ju32(&o, "maxFragmentCombinedOutputResources", l->maxFragmentCombinedOutputResources);
    ju32(&o, "maxComputeSharedMemorySize", l->maxComputeSharedMemorySize);
    ju32arr(&o, "maxComputeWorkGroupCount", l->maxComputeWorkGroupCount, 3);
    ju32(&o, "maxComputeWorkGroupInvocations", l->maxComputeWorkGroupInvocations);
    ju32arr(&o, "maxComputeWorkGroupSize", l->maxComputeWorkGroupSize, 3);
    ju32(&o, "subPixelPrecisionBits", l->subPixelPrecisionBits);
    ju32(&o, "subTexelPrecisionBits", l->subTexelPrecisionBits);
    ju32(&o, "mipmapPrecisionBits", l->mipmapPrecisionBits);
    ju32(&o, "maxDrawIndexedIndexValue", l->maxDrawIndexedIndexValue);
    ju32(&o, "maxDrawIndirectCount", l->maxDrawIndirectCount);
    jf32(&o, "maxSamplerLodBias", l->maxSamplerLodBias);
    jf32(&o, "maxSamplerAnisotropy", l->maxSamplerAnisotropy);
    ju32(&o, "maxViewports", l->maxViewports);
    ju32arr(&o, "maxViewportDimensions", l->maxViewportDimensions, 2);
    jf32arr(&o, "viewportBoundsRange", l->viewportBoundsRange, 2);
    ju32(&o, "viewportSubPixelBits", l->viewportSubPixelBits);
    ju64(&o, "minMemoryMapAlignment", (uint64_t)l->minMemoryMapAlignment);
    ju64(&o, "minTexelBufferOffsetAlignment", l->minTexelBufferOffsetAlignment);
    ju64(&o, "minUniformBufferOffsetAlignment", l->minUniformBufferOffsetAlignment);
    ju64(&o, "minStorageBufferOffsetAlignment", l->minStorageBufferOffsetAlignment);
    ji32(&o, "minTexelOffset", l->minTexelOffset);
    ju32(&o, "maxTexelOffset", l->maxTexelOffset);
    ji32(&o, "minTexelGatherOffset", l->minTexelGatherOffset);
    ju32(&o, "maxTexelGatherOffset", l->maxTexelGatherOffset);
    jf32(&o, "minInterpolationOffset", l->minInterpolationOffset);
    jf32(&o, "maxInterpolationOffset", l->maxInterpolationOffset);
    ju32(&o, "subPixelInterpolationOffsetBits", l->subPixelInterpolationOffsetBits);
    ju32(&o, "maxFramebufferWidth", l->maxFramebufferWidth);
    ju32(&o, "maxFramebufferHeight", l->maxFramebufferHeight);
    ju32(&o, "maxFramebufferLayers", l->maxFramebufferLayers);
    jflags(&o, "framebufferColorSampleCounts", l->framebufferColorSampleCounts);
    jflags(&o, "framebufferDepthSampleCounts", l->framebufferDepthSampleCounts);
    jflags(&o, "framebufferStencilSampleCounts", l->framebufferStencilSampleCounts);
    jflags(&o, "framebufferNoAttachmentsSampleCounts", l->framebufferNoAttachmentsSampleCounts);
    ju32(&o, "maxColorAttachments", l->maxColorAttachments);
    jflags(&o, "sampledImageColorSampleCounts", l->sampledImageColorSampleCounts);
    jflags(&o, "sampledImageIntegerSampleCounts", l->sampledImageIntegerSampleCounts);
    jflags(&o, "sampledImageDepthSampleCounts", l->sampledImageDepthSampleCounts);
    jflags(&o, "sampledImageStencilSampleCounts", l->sampledImageStencilSampleCounts);
    jflags(&o, "storageImageSampleCounts", l->storageImageSampleCounts);
    ju32(&o, "maxSampleMaskWords", l->maxSampleMaskWords);
    jbool(&o, "timestampComputeAndGraphics", l->timestampComputeAndGraphics);
    jf32(&o, "timestampPeriod", l->timestampPeriod);
    ju32(&o, "maxClipDistances", l->maxClipDistances);
    ju32(&o, "maxCullDistances", l->maxCullDistances);
    ju32(&o, "maxCombinedClipAndCullDistances", l->maxCombinedClipAndCullDistances);
    ju32(&o, "discreteQueuePriorities", l->discreteQueuePriorities);
    jf32arr(&o, "pointSizeRange", l->pointSizeRange, 2);
    jf32arr(&o, "lineWidthRange", l->lineWidthRange, 2);
    jf32(&o, "pointSizeGranularity", l->pointSizeGranularity);
    jf32(&o, "lineWidthGranularity", l->lineWidthGranularity);
    jbool(&o, "strictLines", l->strictLines);
    jbool(&o, "standardSampleLocations", l->standardSampleLocations);
    ju64(&o, "optimalBufferCopyOffsetAlignment", l->optimalBufferCopyOffsetAlignment);
    ju64(&o, "optimalBufferCopyRowPitchAlignment", l->optimalBufferCopyRowPitchAlignment);
    ju64(&o, "nonCoherentAtomSize", l->nonCoherentAtomSize);
    jclose(o, '}');
}

static void emit_features(int *f, const VkPhysicalDeviceFeatures *x) {
    int o = 1;
    jopen(f, "features", '{');
    // Every one of the 55, by name and in header order. Phase 4.8 clamps textureCompressionBC and
    // multiViewport; geometryShader, tessellationShader and wideLines must never be forced ON — a
    // feature reported TRUE that the host lacks makes the guest put it in VkDeviceCreateInfo and
    // vkCreateDevice fails outright, i.e. no graphics at all. This block is what proves which way
    // each one actually points on this host.
    jbool(&o, "robustBufferAccess", x->robustBufferAccess);
    jbool(&o, "fullDrawIndexUint32", x->fullDrawIndexUint32);
    jbool(&o, "imageCubeArray", x->imageCubeArray);
    jbool(&o, "independentBlend", x->independentBlend);
    jbool(&o, "geometryShader", x->geometryShader);
    jbool(&o, "tessellationShader", x->tessellationShader);
    jbool(&o, "sampleRateShading", x->sampleRateShading);
    jbool(&o, "dualSrcBlend", x->dualSrcBlend);
    jbool(&o, "logicOp", x->logicOp);
    jbool(&o, "multiDrawIndirect", x->multiDrawIndirect);
    jbool(&o, "drawIndirectFirstInstance", x->drawIndirectFirstInstance);
    jbool(&o, "depthClamp", x->depthClamp);
    jbool(&o, "depthBiasClamp", x->depthBiasClamp);
    jbool(&o, "fillModeNonSolid", x->fillModeNonSolid);
    jbool(&o, "depthBounds", x->depthBounds);
    jbool(&o, "wideLines", x->wideLines);
    jbool(&o, "largePoints", x->largePoints);
    jbool(&o, "alphaToOne", x->alphaToOne);
    jbool(&o, "multiViewport", x->multiViewport);
    jbool(&o, "samplerAnisotropy", x->samplerAnisotropy);
    jbool(&o, "textureCompressionETC2", x->textureCompressionETC2);
    jbool(&o, "textureCompressionASTC_LDR", x->textureCompressionASTC_LDR);
    jbool(&o, "textureCompressionBC", x->textureCompressionBC);
    jbool(&o, "occlusionQueryPrecise", x->occlusionQueryPrecise);
    jbool(&o, "pipelineStatisticsQuery", x->pipelineStatisticsQuery);
    jbool(&o, "vertexPipelineStoresAndAtomics", x->vertexPipelineStoresAndAtomics);
    jbool(&o, "fragmentStoresAndAtomics", x->fragmentStoresAndAtomics);
    jbool(&o, "shaderTessellationAndGeometryPointSize", x->shaderTessellationAndGeometryPointSize);
    jbool(&o, "shaderImageGatherExtended", x->shaderImageGatherExtended);
    jbool(&o, "shaderStorageImageExtendedFormats", x->shaderStorageImageExtendedFormats);
    jbool(&o, "shaderStorageImageMultisample", x->shaderStorageImageMultisample);
    jbool(&o, "shaderStorageImageReadWithoutFormat", x->shaderStorageImageReadWithoutFormat);
    jbool(&o, "shaderStorageImageWriteWithoutFormat", x->shaderStorageImageWriteWithoutFormat);
    jbool(&o, "shaderUniformBufferArrayDynamicIndexing", x->shaderUniformBufferArrayDynamicIndexing);
    jbool(&o, "shaderSampledImageArrayDynamicIndexing", x->shaderSampledImageArrayDynamicIndexing);
    jbool(&o, "shaderStorageBufferArrayDynamicIndexing", x->shaderStorageBufferArrayDynamicIndexing);
    jbool(&o, "shaderStorageImageArrayDynamicIndexing", x->shaderStorageImageArrayDynamicIndexing);
    jbool(&o, "shaderClipDistance", x->shaderClipDistance);
    jbool(&o, "shaderCullDistance", x->shaderCullDistance);
    jbool(&o, "shaderFloat64", x->shaderFloat64);
    jbool(&o, "shaderInt64", x->shaderInt64);
    jbool(&o, "shaderInt16", x->shaderInt16);
    jbool(&o, "shaderResourceResidency", x->shaderResourceResidency);
    jbool(&o, "shaderResourceMinLod", x->shaderResourceMinLod);
    jbool(&o, "sparseBinding", x->sparseBinding);
    jbool(&o, "sparseResidencyBuffer", x->sparseResidencyBuffer);
    jbool(&o, "sparseResidencyImage2D", x->sparseResidencyImage2D);
    jbool(&o, "sparseResidencyImage3D", x->sparseResidencyImage3D);
    jbool(&o, "sparseResidency2Samples", x->sparseResidency2Samples);
    jbool(&o, "sparseResidency4Samples", x->sparseResidency4Samples);
    jbool(&o, "sparseResidency8Samples", x->sparseResidency8Samples);
    jbool(&o, "sparseResidency16Samples", x->sparseResidency16Samples);
    jbool(&o, "sparseResidencyAliased", x->sparseResidencyAliased);
    jbool(&o, "variableMultisampleRate", x->variableMultisampleRate);
    jbool(&o, "inheritedQueries", x->inheritedQueries);
    jclose(o, '}');
}

static void emit_vk11(int *f, const VkPhysicalDeviceVulkan11Properties *p) {
    int o = 1;
    jopen(f, "vulkan11Properties", '{');
    jhexbytes(&o, "deviceUUID", p->deviceUUID, VK_UUID_SIZE);
    jhexbytes(&o, "driverUUID", p->driverUUID, VK_UUID_SIZE);
    jhexbytes(&o, "deviceLUID", p->deviceLUID, VK_LUID_SIZE);
    ju32(&o, "deviceNodeMask", p->deviceNodeMask);
    jbool(&o, "deviceLUIDValid", p->deviceLUIDValid);
    ju32(&o, "subgroupSize", p->subgroupSize);
    jflags(&o, "subgroupSupportedStages", p->subgroupSupportedStages);
    jflags(&o, "subgroupSupportedOperations", p->subgroupSupportedOperations);
    jbool(&o, "subgroupQuadOperationsInAllStages", p->subgroupQuadOperationsInAllStages);
    ju32(&o, "pointClippingBehavior", (uint32_t)p->pointClippingBehavior);
    ju32(&o, "maxMultiviewViewCount", p->maxMultiviewViewCount);
    ju32(&o, "maxMultiviewInstanceIndex", p->maxMultiviewInstanceIndex);
    jbool(&o, "protectedNoFault", p->protectedNoFault);
    ju32(&o, "maxPerSetDescriptors", p->maxPerSetDescriptors);
    ju64(&o, "maxMemoryAllocationSize", p->maxMemoryAllocationSize);
    jclose(o, '}');
}

static void emit_vk12(int *f, const VkPhysicalDeviceVulkan12Properties *p) {
    int o = 1;
    jopen(f, "vulkan12Properties", '{');
    ju32(&o, "driverID", (uint32_t)p->driverID);
    jstr(&o, "driverIDName", driver_id_name(p->driverID));
    jstr(&o, "driverName", p->driverName);
    jstr(&o, "driverInfo", p->driverInfo);
    {
        int c = 1;
        jopen(&o, "conformanceVersion", '{');
        ju32(&c, "major", p->conformanceVersion.major);
        ju32(&c, "minor", p->conformanceVersion.minor);
        ju32(&c, "subminor", p->conformanceVersion.subminor);
        ju32(&c, "patch", p->conformanceVersion.patch);
        jclose(c, '}');
    }
    ju32(&o, "denormBehaviorIndependence", (uint32_t)p->denormBehaviorIndependence);
    ju32(&o, "roundingModeIndependence", (uint32_t)p->roundingModeIndependence);
    jbool(&o, "shaderSignedZeroInfNanPreserveFloat16", p->shaderSignedZeroInfNanPreserveFloat16);
    jbool(&o, "shaderSignedZeroInfNanPreserveFloat32", p->shaderSignedZeroInfNanPreserveFloat32);
    jbool(&o, "shaderSignedZeroInfNanPreserveFloat64", p->shaderSignedZeroInfNanPreserveFloat64);
    jbool(&o, "shaderDenormPreserveFloat16", p->shaderDenormPreserveFloat16);
    jbool(&o, "shaderDenormPreserveFloat32", p->shaderDenormPreserveFloat32);
    jbool(&o, "shaderDenormPreserveFloat64", p->shaderDenormPreserveFloat64);
    jbool(&o, "shaderDenormFlushToZeroFloat16", p->shaderDenormFlushToZeroFloat16);
    jbool(&o, "shaderDenormFlushToZeroFloat32", p->shaderDenormFlushToZeroFloat32);
    jbool(&o, "shaderDenormFlushToZeroFloat64", p->shaderDenormFlushToZeroFloat64);
    jbool(&o, "shaderRoundingModeRTEFloat16", p->shaderRoundingModeRTEFloat16);
    jbool(&o, "shaderRoundingModeRTEFloat32", p->shaderRoundingModeRTEFloat32);
    jbool(&o, "shaderRoundingModeRTEFloat64", p->shaderRoundingModeRTEFloat64);
    jbool(&o, "shaderRoundingModeRTZFloat16", p->shaderRoundingModeRTZFloat16);
    jbool(&o, "shaderRoundingModeRTZFloat32", p->shaderRoundingModeRTZFloat32);
    jbool(&o, "shaderRoundingModeRTZFloat64", p->shaderRoundingModeRTZFloat64);
    ju32(&o, "maxUpdateAfterBindDescriptorsInAllPools", p->maxUpdateAfterBindDescriptorsInAllPools);
    jbool(&o, "shaderUniformBufferArrayNonUniformIndexingNative", p->shaderUniformBufferArrayNonUniformIndexingNative);
    jbool(&o, "shaderSampledImageArrayNonUniformIndexingNative", p->shaderSampledImageArrayNonUniformIndexingNative);
    jbool(&o, "shaderStorageBufferArrayNonUniformIndexingNative", p->shaderStorageBufferArrayNonUniformIndexingNative);
    jbool(&o, "shaderStorageImageArrayNonUniformIndexingNative", p->shaderStorageImageArrayNonUniformIndexingNative);
    jbool(&o, "shaderInputAttachmentArrayNonUniformIndexingNative", p->shaderInputAttachmentArrayNonUniformIndexingNative);
    jbool(&o, "robustBufferAccessUpdateAfterBind", p->robustBufferAccessUpdateAfterBind);
    jbool(&o, "quadDivergentImplicitLod", p->quadDivergentImplicitLod);
    ju32(&o, "maxPerStageDescriptorUpdateAfterBindSamplers", p->maxPerStageDescriptorUpdateAfterBindSamplers);
    ju32(&o, "maxPerStageDescriptorUpdateAfterBindUniformBuffers", p->maxPerStageDescriptorUpdateAfterBindUniformBuffers);
    ju32(&o, "maxPerStageDescriptorUpdateAfterBindStorageBuffers", p->maxPerStageDescriptorUpdateAfterBindStorageBuffers);
    ju32(&o, "maxPerStageDescriptorUpdateAfterBindSampledImages", p->maxPerStageDescriptorUpdateAfterBindSampledImages);
    ju32(&o, "maxPerStageDescriptorUpdateAfterBindStorageImages", p->maxPerStageDescriptorUpdateAfterBindStorageImages);
    ju32(&o, "maxPerStageDescriptorUpdateAfterBindInputAttachments", p->maxPerStageDescriptorUpdateAfterBindInputAttachments);
    ju32(&o, "maxPerStageUpdateAfterBindResources", p->maxPerStageUpdateAfterBindResources);
    ju32(&o, "maxDescriptorSetUpdateAfterBindSamplers", p->maxDescriptorSetUpdateAfterBindSamplers);
    ju32(&o, "maxDescriptorSetUpdateAfterBindUniformBuffers", p->maxDescriptorSetUpdateAfterBindUniformBuffers);
    ju32(&o, "maxDescriptorSetUpdateAfterBindUniformBuffersDynamic", p->maxDescriptorSetUpdateAfterBindUniformBuffersDynamic);
    ju32(&o, "maxDescriptorSetUpdateAfterBindStorageBuffers", p->maxDescriptorSetUpdateAfterBindStorageBuffers);
    ju32(&o, "maxDescriptorSetUpdateAfterBindStorageBuffersDynamic", p->maxDescriptorSetUpdateAfterBindStorageBuffersDynamic);
    ju32(&o, "maxDescriptorSetUpdateAfterBindSampledImages", p->maxDescriptorSetUpdateAfterBindSampledImages);
    ju32(&o, "maxDescriptorSetUpdateAfterBindStorageImages", p->maxDescriptorSetUpdateAfterBindStorageImages);
    ju32(&o, "maxDescriptorSetUpdateAfterBindInputAttachments", p->maxDescriptorSetUpdateAfterBindInputAttachments);
    jflags(&o, "supportedDepthResolveModes", p->supportedDepthResolveModes);
    jflags(&o, "supportedStencilResolveModes", p->supportedStencilResolveModes);
    jbool(&o, "independentResolveNone", p->independentResolveNone);
    jbool(&o, "independentResolve", p->independentResolve);
    jbool(&o, "filterMinmaxSingleComponentFormats", p->filterMinmaxSingleComponentFormats);
    jbool(&o, "filterMinmaxImageComponentMapping", p->filterMinmaxImageComponentMapping);
    ju64(&o, "maxTimelineSemaphoreValueDifference", p->maxTimelineSemaphoreValueDifference);
    jflags(&o, "framebufferIntegerColorSampleCounts", p->framebufferIntegerColorSampleCounts);
    jclose(o, '}');
}

static void emit_vk13(int *f, const VkPhysicalDeviceVulkan13Properties *p) {
    int o = 1;
    jopen(f, "vulkan13Properties", '{');
    ju32(&o, "minSubgroupSize", p->minSubgroupSize);
    ju32(&o, "maxSubgroupSize", p->maxSubgroupSize);
    ju32(&o, "maxComputeWorkgroupSubgroups", p->maxComputeWorkgroupSubgroups);
    jflags(&o, "requiredSubgroupSizeStages", p->requiredSubgroupSizeStages);
    ju32(&o, "maxInlineUniformBlockSize", p->maxInlineUniformBlockSize);
    ju32(&o, "maxPerStageDescriptorInlineUniformBlocks", p->maxPerStageDescriptorInlineUniformBlocks);
    ju32(&o, "maxPerStageDescriptorUpdateAfterBindInlineUniformBlocks", p->maxPerStageDescriptorUpdateAfterBindInlineUniformBlocks);
    ju32(&o, "maxDescriptorSetInlineUniformBlocks", p->maxDescriptorSetInlineUniformBlocks);
    ju32(&o, "maxDescriptorSetUpdateAfterBindInlineUniformBlocks", p->maxDescriptorSetUpdateAfterBindInlineUniformBlocks);
    ju32(&o, "maxInlineUniformTotalSize", p->maxInlineUniformTotalSize);
    jbool(&o, "integerDotProduct8BitUnsignedAccelerated", p->integerDotProduct8BitUnsignedAccelerated);
    jbool(&o, "integerDotProduct8BitSignedAccelerated", p->integerDotProduct8BitSignedAccelerated);
    jbool(&o, "integerDotProduct8BitMixedSignednessAccelerated", p->integerDotProduct8BitMixedSignednessAccelerated);
    jbool(&o, "integerDotProduct4x8BitPackedUnsignedAccelerated", p->integerDotProduct4x8BitPackedUnsignedAccelerated);
    jbool(&o, "integerDotProduct4x8BitPackedSignedAccelerated", p->integerDotProduct4x8BitPackedSignedAccelerated);
    jbool(&o, "integerDotProduct4x8BitPackedMixedSignednessAccelerated", p->integerDotProduct4x8BitPackedMixedSignednessAccelerated);
    jbool(&o, "integerDotProduct16BitUnsignedAccelerated", p->integerDotProduct16BitUnsignedAccelerated);
    jbool(&o, "integerDotProduct16BitSignedAccelerated", p->integerDotProduct16BitSignedAccelerated);
    jbool(&o, "integerDotProduct16BitMixedSignednessAccelerated", p->integerDotProduct16BitMixedSignednessAccelerated);
    jbool(&o, "integerDotProduct32BitUnsignedAccelerated", p->integerDotProduct32BitUnsignedAccelerated);
    jbool(&o, "integerDotProduct32BitSignedAccelerated", p->integerDotProduct32BitSignedAccelerated);
    jbool(&o, "integerDotProduct32BitMixedSignednessAccelerated", p->integerDotProduct32BitMixedSignednessAccelerated);
    jbool(&o, "integerDotProduct64BitUnsignedAccelerated", p->integerDotProduct64BitUnsignedAccelerated);
    jbool(&o, "integerDotProduct64BitSignedAccelerated", p->integerDotProduct64BitSignedAccelerated);
    jbool(&o, "integerDotProduct64BitMixedSignednessAccelerated", p->integerDotProduct64BitMixedSignednessAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating8BitUnsignedAccelerated", p->integerDotProductAccumulatingSaturating8BitUnsignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating8BitSignedAccelerated", p->integerDotProductAccumulatingSaturating8BitSignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating8BitMixedSignednessAccelerated", p->integerDotProductAccumulatingSaturating8BitMixedSignednessAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating4x8BitPackedUnsignedAccelerated", p->integerDotProductAccumulatingSaturating4x8BitPackedUnsignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating4x8BitPackedSignedAccelerated", p->integerDotProductAccumulatingSaturating4x8BitPackedSignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating4x8BitPackedMixedSignednessAccelerated", p->integerDotProductAccumulatingSaturating4x8BitPackedMixedSignednessAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating16BitUnsignedAccelerated", p->integerDotProductAccumulatingSaturating16BitUnsignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating16BitSignedAccelerated", p->integerDotProductAccumulatingSaturating16BitSignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating16BitMixedSignednessAccelerated", p->integerDotProductAccumulatingSaturating16BitMixedSignednessAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating32BitUnsignedAccelerated", p->integerDotProductAccumulatingSaturating32BitUnsignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating32BitSignedAccelerated", p->integerDotProductAccumulatingSaturating32BitSignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating32BitMixedSignednessAccelerated", p->integerDotProductAccumulatingSaturating32BitMixedSignednessAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating64BitUnsignedAccelerated", p->integerDotProductAccumulatingSaturating64BitUnsignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating64BitSignedAccelerated", p->integerDotProductAccumulatingSaturating64BitSignedAccelerated);
    jbool(&o, "integerDotProductAccumulatingSaturating64BitMixedSignednessAccelerated", p->integerDotProductAccumulatingSaturating64BitMixedSignednessAccelerated);
    ju64(&o, "storageTexelBufferOffsetAlignmentBytes", p->storageTexelBufferOffsetAlignmentBytes);
    jbool(&o, "storageTexelBufferOffsetSingleTexelAlignment", p->storageTexelBufferOffsetSingleTexelAlignment);
    ju64(&o, "uniformTexelBufferOffsetAlignmentBytes", p->uniformTexelBufferOffsetAlignmentBytes);
    jbool(&o, "uniformTexelBufferOffsetSingleTexelAlignment", p->uniformTexelBufferOffsetSingleTexelAlignment);
    ju64(&o, "maxBufferSize", p->maxBufferSize);
    jclose(o, '}');
}

static int dump_identity(void) {
    // The loader's own version is provenance, not device identity: a `brew upgrade vulkan-loader`
    // legitimately moves it and that SHOULD show up as a diff. The instance is created at the same
    // 1.3 the gate asks for, so the two modes see the driver through the same window.
    uint32_t loaderVersion = VK_API_VERSION_1_0;
    PFN_vkEnumerateInstanceVersion eiv =
        (PFN_vkEnumerateInstanceVersion)vkGetInstanceProcAddr(NULL, "vkEnumerateInstanceVersion");
    if (eiv) DCHECK(eiv(&loaderVersion));

    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "kk-gate",
                             .apiVersion = VK_API_VERSION_1_3};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance inst;
    DCHECK(vkCreateInstance(&ici, NULL, &inst));

    uint32_t n = 0;
    DCHECK(vkEnumeratePhysicalDevices(inst, &n, NULL));
    if (!n) DFAIL("FAIL no physical devices\n");
    VkPhysicalDevice *pds = calloc(n, sizeof *pds);
    if (!pds) DFAIL("FAIL out of memory\n");
    DCHECK(vkEnumeratePhysicalDevices(inst, &n, pds));
    VkPhysicalDevice pd = pds[0]; // same choice the gate makes, so both modes describe one device

    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);

    // Which promoted property structs may be chained is decided by the DEVICE's apiVersion, not the
    // instance's: Vulkan11/12Properties arrived in 1.2 and Vulkan13Properties in 1.3. Chaining one a
    // 1.1 device does not know about is undefined behaviour, so the version gate is mandatory, and
    // the skipped members emit `null` rather than vanishing — a stable key set is what makes two
    // dumps from different hosts diffable at all.
    int have12 = props.apiVersion >= VK_API_VERSION_1_2;
    int have13 = props.apiVersion >= VK_API_VERSION_1_3;

    VkPhysicalDeviceDriverProperties drv = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES};
    VkPhysicalDeviceIDProperties idp = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES, .pNext = &drv};
    VkPhysicalDeviceSubgroupProperties sub = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SUBGROUP_PROPERTIES,
                                              .pNext = &idp};
    VkPhysicalDeviceVulkan11Properties v11 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_PROPERTIES};
    VkPhysicalDeviceVulkan12Properties v12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_PROPERTIES};
    VkPhysicalDeviceVulkan13Properties v13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_PROPERTIES};
    void *chain = &sub;
    if (have12) {
        v11.pNext = chain;
        v12.pNext = &v11;
        chain = &v12;
    }
    if (have13) {
        v13.pNext = chain;
        chain = &v13;
    }
    VkPhysicalDeviceProperties2 p2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = chain};
    vkGetPhysicalDeviceProperties2(pd, &p2);

    VkPhysicalDeviceFeatures feats;
    vkGetPhysicalDeviceFeatures(pd, &feats);

    uint32_t en = 0;
    DCHECK(vkEnumerateDeviceExtensionProperties(pd, NULL, &en, NULL));
    VkExtensionProperties *exts = en ? calloc(en, sizeof *exts) : NULL;
    if (en && !exts) DFAIL("FAIL out of memory\n");
    if (en) DCHECK(vkEnumerateDeviceExtensionProperties(pd, NULL, &en, exts));
    // The loader returns these in whatever order the ICD listed them. Sort so the golden does not
    // churn on an unrelated driver rebuild.
    if (en) qsort(exts, en, sizeof *exts, extcmp);

    // A bare device — no extensions, no features — is all vkCreatePipelineCache needs, and asking for
    // nothing means this cannot fail for a reason that has nothing to do with identity.
    uint32_t qn = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, NULL);
    VkQueueFamilyProperties *qs = calloc(qn ? qn : 1, sizeof *qs);
    if (!qs) DFAIL("FAIL out of memory\n");
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, qs);
    uint32_t qfi = UINT32_MAX;
    for (uint32_t i = 0; i < qn; i++)
        if (qs[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) { qfi = i; break; }
    if (qfi == UINT32_MAX) DFAIL("FAIL no graphics queue family\n");

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueFamilyIndex = qfi, .queueCount = 1, .pQueuePriorities = &prio};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
    VkDevice dev;
    DCHECK(vkCreateDevice(pd, &dci, NULL, &dev));

    VkPipelineCacheCreateInfo pcci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO};
    VkPipelineCache cache;
    DCHECK(vkCreatePipelineCache(dev, &pcci, NULL, &cache));
    size_t cacheSize = 0;
    DCHECK(vkGetPipelineCacheData(dev, cache, &cacheSize, NULL));
    uint8_t hdr[32];
    memset(hdr, 0, sizeof hdr);
    size_t got = 0;
    if (cacheSize) {
        uint8_t *blob = malloc(cacheSize);
        if (!blob) DFAIL("FAIL out of memory\n");
        size_t sz = cacheSize;
        DCHECK(vkGetPipelineCacheData(dev, cache, &sz, blob));
        got = sz < sizeof hdr ? sz : sizeof hdr;
        memcpy(hdr, blob, got);
        free(blob);
    }

    // ---- emit ------------------------------------------------------------
    // Every fallible query is above this line, so a driver that fails prints to stderr and exits
    // non-zero having written nothing at all. Partial JSON on stdout is not a possible outcome. The
    // only Vulkan calls below return void.
    int root = 1;
    printf("{");
    jdepth++;
    jstr(&root, "schema", "aemulator/host-vulkan-identity@1");
    jstr(&root, "producer", "crates/aemulator-hvf/kosmickrisp/kk-gate.c --dump-identity");
    jstr(&root, "note",
         "Phase-0.6 identity ground truth for the HOST Vulkan driver. Deliberately no timestamp, no "
         "pointer, no filesystem path: a rerun on an unchanged host must be byte-identical, so any "
         "diff is a real change. See gfxstream/evidence/README.md.");
    ju32(&root, "headerVersion", VK_HEADER_VERSION);
    jver(&root, "loaderInstanceApiVersion", loaderVersion);
    jver(&root, "requestedInstanceApiVersion", VK_API_VERSION_1_3);
    ju32(&root, "physicalDeviceCount", n);
    ju32(&root, "physicalDeviceIndex", 0);
    {
        int a = 1;
        jopen(&root, "physicalDeviceNames", '[');
        for (uint32_t i = 0; i < n; i++) {
            VkPhysicalDeviceProperties o;
            vkGetPhysicalDeviceProperties(pds[i], &o);
            jstr(&a, NULL, o.deviceName);
        }
        jclose(a, ']');
    }

    {
        int o = 1;
        jopen(&root, "properties", '{');
        ju32(&o, "apiVersion", props.apiVersion);
        jver(&o, "apiVersionText", props.apiVersion);
        ju32(&o, "driverVersion", props.driverVersion);
        // driverVersion's encoding is vendor-defined; the Vulkan major.minor.patch split is only the
        // conventional reading. Both forms are recorded so a future reader is not guessing.
        jver(&o, "driverVersionVulkanConvention", props.driverVersion);
        ju32(&o, "vendorID", props.vendorID);
        jhex32(&o, "vendorIDHex", props.vendorID);
        ju32(&o, "deviceID", props.deviceID);
        jhex32(&o, "deviceIDHex", props.deviceID);
        ju32(&o, "deviceType", (uint32_t)props.deviceType);
        jstr(&o, "deviceTypeName", device_type_name(props.deviceType));
        jstr(&o, "deviceName", props.deviceName);
        jhexbytes(&o, "pipelineCacheUUID", props.pipelineCacheUUID, VK_UUID_SIZE);
        emit_limits(&o, &props.limits);
        {
            int s = 1;
            jopen(&o, "sparseProperties", '{');
            jbool(&s, "residencyStandard2DBlockShape", props.sparseProperties.residencyStandard2DBlockShape);
            jbool(&s, "residencyStandard2DMultisampleBlockShape",
                  props.sparseProperties.residencyStandard2DMultisampleBlockShape);
            jbool(&s, "residencyStandard3DBlockShape", props.sparseProperties.residencyStandard3DBlockShape);
            jbool(&s, "residencyAlignedMipSize", props.sparseProperties.residencyAlignedMipSize);
            jbool(&s, "residencyNonResidentStrict", props.sparseProperties.residencyNonResidentStrict);
            jclose(s, '}');
        }
        jclose(o, '}');
    }

    {
        int o = 1;
        jopen(&root, "driverProperties", '{');
        ju32(&o, "driverID", (uint32_t)drv.driverID);
        jstr(&o, "driverIDName", driver_id_name(drv.driverID));
        jstr(&o, "driverName", drv.driverName);
        jstr(&o, "driverInfo", drv.driverInfo);
        {
            int c = 1;
            jopen(&o, "conformanceVersion", '{');
            ju32(&c, "major", drv.conformanceVersion.major);
            ju32(&c, "minor", drv.conformanceVersion.minor);
            ju32(&c, "subminor", drv.conformanceVersion.subminor);
            ju32(&c, "patch", drv.conformanceVersion.patch);
            jclose(c, '}');
        }
        jclose(o, '}');
    }

    {
        int o = 1;
        jopen(&root, "idProperties", '{');
        jhexbytes(&o, "deviceUUID", idp.deviceUUID, VK_UUID_SIZE);
        jhexbytes(&o, "driverUUID", idp.driverUUID, VK_UUID_SIZE);
        jhexbytes(&o, "deviceLUID", idp.deviceLUID, VK_LUID_SIZE);
        ju32(&o, "deviceNodeMask", idp.deviceNodeMask);
        jbool(&o, "deviceLUIDValid", idp.deviceLUIDValid);
        jclose(o, '}');
    }

    {
        int o = 1;
        jopen(&root, "subgroupProperties", '{');
        ju32(&o, "subgroupSize", sub.subgroupSize);
        jflags(&o, "supportedStages", sub.supportedStages);
        jflags(&o, "supportedOperations", sub.supportedOperations);
        jbool(&o, "quadOperationsInAllStages", sub.quadOperationsInAllStages);
        jclose(o, '}');
    }

    if (have12) { emit_vk11(&root, &v11); emit_vk12(&root, &v12); }
    else { jnull(&root, "vulkan11Properties"); jnull(&root, "vulkan12Properties"); }
    if (have13) emit_vk13(&root, &v13);
    else jnull(&root, "vulkan13Properties");

    emit_features(&root, &feats);

    ju32(&root, "deviceExtensionCount", en);
    {
        int a = 1;
        jopen(&root, "deviceExtensions", '[');
        for (uint32_t i = 0; i < en; i++) {
            int e = 1;
            jopen(&a, NULL, '{');
            jstr(&e, "name", exts[i].extensionName);
            ju32(&e, "specVersion", exts[i].specVersion);
            jclose(e, '}');
        }
        jclose(a, ']');
    }

    {
        int a = 1;
        jopen(&root, "blockCompressedFormatProperties", '[');
        for (size_t i = 0; i < sizeof kBcFormats / sizeof *kBcFormats; i++) {
            VkFormatProperties fp;
            vkGetPhysicalDeviceFormatProperties(pd, kBcFormats[i].fmt, &fp);
            int e = 1;
            jopen(&a, NULL, '{');
            jstr(&e, "format", kBcFormats[i].name);
            ju32(&e, "value", (uint32_t)kBcFormats[i].fmt);
            jflags(&e, "linearTilingFeatures", fp.linearTilingFeatures);
            jflags(&e, "optimalTilingFeatures", fp.optimalTilingFeatures);
            jflags(&e, "bufferFeatures", fp.bufferFeatures);
            jclose(e, '}');
        }
        jclose(a, ']');
    }

    {
        int o = 1;
        jopen(&root, "pipelineCacheHeader", '{');
        jstr(&o, "why",
             "vkGetPipelineCacheData is a RAW PASSTHROUGH in gfxstream (no on_vkGetPipelineCacheData "
             "in VkDecoderGlobalState; VkDecoder.cpp:4403 calls the driver). Mesa builds this 32-byte "
             "VkPipelineCacheHeaderVersionOne from the real vendorID (bytes 8..12), deviceID (12..16) "
             "and pipelineCacheUUID (16..32), so any guest app recovers all three from a cache blob no "
             "matter what the properties call reported. Phase 4.9 rewrites bytes 8..32 outbound and "
             "back inbound; after that, the *AgreesWithProperties flags below must read false.");
        ju64(&o, "totalCacheDataSize", (uint64_t)cacheSize);
        ju64(&o, "bytesCaptured", (uint64_t)got);
        jhexbytes(&o, "first32BytesHex", hdr, sizeof hdr);
        {
            int d = 1;
            jopen(&o, "decoded", '{');
            ju32(&d, "headerSize_bytes0_4", rd_le32(hdr + 0));
            ju32(&d, "headerVersion_bytes4_8", rd_le32(hdr + 4));
            ju32(&d, "vendorID_bytes8_12", rd_le32(hdr + 8));
            jhex32(&d, "vendorID_bytes8_12_hex", rd_le32(hdr + 8));
            ju32(&d, "deviceID_bytes12_16", rd_le32(hdr + 12));
            jhex32(&d, "deviceID_bytes12_16_hex", rd_le32(hdr + 12));
            jhexbytes(&d, "pipelineCacheUUID_bytes16_32", hdr + 16, 16);
            jclose(d, '}');
        }
        {
            int m = 1;
            jopen(&o, "leak", '{');
            jbool(&m, "vendorIDAgreesWithProperties", rd_le32(hdr + 8) == props.vendorID);
            jbool(&m, "deviceIDAgreesWithProperties", rd_le32(hdr + 12) == props.deviceID);
            jbool(&m, "pipelineCacheUUIDAgreesWithProperties",
                  memcmp(hdr + 16, props.pipelineCacheUUID, VK_UUID_SIZE) == 0);
            jclose(m, '}');
        }
        jclose(o, '}');
    }

    jdepth--;
    printf("\n}\n");

    vkDestroyPipelineCache(dev, cache, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    free(exts);
    free(qs);
    free(pds);
    return 0;
}

int main(int argc, char **argv) {
    // No arguments is the gate, byte-for-byte as before. Anything unrecognised is a usage error on
    // stderr rather than a silent fallback to the gate — a typo'd flag that quietly ran the wrong
    // mode would look like a passing capability test and produce no dump.
    if (argc > 1) {
        if (strcmp(argv[1], "--dump-identity") == 0) return dump_identity();
        fprintf(stderr, "usage: %s [--dump-identity]\n", argv[0]);
        return 2;
    }
    return run_gate();
}
