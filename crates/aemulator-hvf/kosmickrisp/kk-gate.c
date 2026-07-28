// kk-gate.c — Phase 2 gate for the MoltenVK -> KosmicKrisp migration.
//
// vulkaninfo only proves the loader can enumerate the driver. This proves the three things gfxstream
// actually stands on, and nothing else:
//   1. Device creation with VK_EXT_external_memory_metal enabled.
//   2. An external-memory VkImage exported as an MTLHeap — gfxstream's ColorBuffer path uses exactly
//      VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT, and KosmicKrisp supports *only* that handle type.
//   3. Real queue submission reaching the GPU (clear + fence wait), i.e. Metal encoding works.
//
// Build: clang -o kk-gate kk-gate.c -I$(brew --prefix vulkan-headers)/include -L/opt/homebrew/lib -lvulkan
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

int main(void) {
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
