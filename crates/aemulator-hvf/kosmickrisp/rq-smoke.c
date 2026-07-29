/* rq-smoke.c — exercise VK_KHR_ray_query on KosmicKrisp end to end.
 *
 * as-smoke.c stops at "the acceleration structures build". This goes the rest of
 * the way: it builds the same BLAS+TLAS, compiles a compute shader that does a
 * real rayQueryEXT against the TLAS, dispatches it, and reads the intersection
 * back. Eight invocations shoot parallel rays down +Z from (0.25 + i, 0.25, 0);
 * the one triangle in the BLAS spans (0,0,1) (1,0,1) (0,1,1), so ray 0 must hit
 * at t = 1 with barycentrics (0.25, 0.25) and rays 1..7 must miss. Both halves
 * are checked, because a driver that reports "hit" for everything and one that
 * reports "miss" for everything are equally broken and a hit-only test cannot
 * tell them apart.
 *
 * EXPECT 12/16 ON AN APPLE M2 (Apple8), and expect the four failures to be
 * exactly the four hit-path checks. That is not this harness being wrong and it
 * is not the driver's shader path being wrong: the M2 does not implement Metal 4
 * ray tracing, so an acceleration structure built through
 * -[MTL4ComputeCommandEncoder buildAccelerationStructure:...] — which is the
 * only builder a Metal 4 driver can reach — intersects nothing. Metal API
 * validation states it outright; run mtl4-rt-matrix under
 * METAL_DEVICE_WRAPPER_TYPE=1 to see it, and without validation to see the same
 * BLAS+TLAS built on the legacy MTLAccelerationStructureCommandEncoder hit
 * correctly. So on Apple8 the honest reading of a 12/16 is:
 *
 *    advertisement, features, VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR,
 *    SPIR-V OpRayQuery* -> NIR -> MSL, pipeline creation, dispatch, the ray
 *    flags/tmin accessors and the MISS path all work; the hit path is blocked
 *    below the driver.
 *
 * On Apple9 (M3) and later the four hit checks are the ones that say whether
 * ray tracing actually works, and 16/16 is the bar.
 *
 * The values checked are the ones Metal can answer. Not checked:
 *   - instanceShaderBindingTableRecordOffset (no shader binding table exists)
 *   - rayQueryGetIntersectionTriangleVertexPositionsEXT (position fetch is a
 *     separate extension and is deliberately not advertised)
 */
#include <vulkan/vulkan.h>

#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "rqsmoke_spv.h"

#define CHECK(x)                                                               \
   do {                                                                        \
      VkResult _r = (x);                                                        \
      if (_r != VK_SUCCESS) {                                                   \
         printf("FAIL %s -> %d\n", #x, _r);                                     \
         exit(1);                                                               \
      }                                                                         \
   } while (0)

#define NRAYS 8

static VkDevice dev;
static VkPhysicalDevice pdev;
static VkPhysicalDeviceMemoryProperties memprops;

static PFN_vkGetAccelerationStructureBuildSizesKHR pGetSizes;
static PFN_vkCreateAccelerationStructureKHR pCreateAS;
static PFN_vkDestroyAccelerationStructureKHR pDestroyAS;
static PFN_vkGetAccelerationStructureDeviceAddressKHR pGetASAddr;
static PFN_vkCmdBuildAccelerationStructuresKHR pCmdBuild;

static int pass_count, fail_count;

static void
report(int ok, const char *fmt, ...)
{
   va_list ap;
   if (ok)
      pass_count++;
   else
      fail_count++;
   printf("%s: ", ok ? "PASS" : "FAIL");
   va_start(ap, fmt);
   vprintf(fmt, ap);
   va_end(ap);
   printf("\n");
}

struct buf {
   VkBuffer buffer;
   VkDeviceMemory mem;
   VkDeviceAddress addr;
   void *map;
};

static struct buf
make_buf(VkDeviceSize size, VkBufferUsageFlags usage)
{
   struct buf b = {0};
   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = usage | VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &b.buffer));

   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, b.buffer, &req);

   uint32_t type = UINT32_MAX;
   for (uint32_t i = 0; i < memprops.memoryTypeCount; ++i) {
      if (!(req.memoryTypeBits & (1u << i)))
         continue;
      VkMemoryPropertyFlags f = memprops.memoryTypes[i].propertyFlags;
      if ((f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) &&
          (f & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) {
         type = i;
         break;
      }
   }
   if (type == UINT32_MAX) {
      printf("FAIL: no host visible memory type\n");
      exit(1);
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
   CHECK(vkAllocateMemory(dev, &ai, NULL, &b.mem));
   CHECK(vkBindBufferMemory(dev, b.buffer, b.mem, 0));
   CHECK(vkMapMemory(dev, b.mem, 0, VK_WHOLE_SIZE, 0, &b.map));

   VkBufferDeviceAddressInfo bdai = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
      .buffer = b.buffer,
   };
   b.addr = vkGetBufferDeviceAddress(dev, &bdai);
   return b;
}

/* Must match Result in shaders/rqsmoke.comp (std430, all 4-byte scalars). */
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

static int
near(float a, float b)
{
   return fabsf(a - b) < 1e-4f;
}

#define CUSTOM_INDEX 0x123456u

int
main(void)
{
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t n = 1;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, &pdev));
   vkGetPhysicalDeviceMemoryProperties(pdev, &memprops);

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);
   printf("device: %s\n", props.deviceName);

   /* ---- 1. VK_KHR_ray_query is advertised ---- */
   uint32_t ext_count = 0;
   vkEnumerateDeviceExtensionProperties(pdev, NULL, &ext_count, NULL);
   VkExtensionProperties *exts = calloc(ext_count, sizeof(*exts));
   vkEnumerateDeviceExtensionProperties(pdev, NULL, &ext_count, exts);
   int has_rq = 0, has_as = 0, has_dho = 0;
   for (uint32_t i = 0; i < ext_count; ++i) {
      if (!strcmp(exts[i].extensionName, VK_KHR_RAY_QUERY_EXTENSION_NAME))
         has_rq = 1;
      if (!strcmp(exts[i].extensionName,
                  VK_KHR_ACCELERATION_STRUCTURE_EXTENSION_NAME))
         has_as = 1;
      if (!strcmp(exts[i].extensionName,
                  VK_KHR_DEFERRED_HOST_OPERATIONS_EXTENSION_NAME))
         has_dho = 1;
   }
   free(exts);
   report(has_rq, "VK_KHR_ray_query advertised");
   report(has_as && has_dho,
          "VK_KHR_acceleration_structure + deferred_host_operations advertised");
   if (!has_rq || !has_as || !has_dho)
      return 1;

   /* ---- 2. the rayQuery feature bit ---- */
   VkPhysicalDeviceRayQueryFeaturesKHR rqfeat = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_RAY_QUERY_FEATURES_KHR,
   };
   VkPhysicalDeviceAccelerationStructureFeaturesKHR asfeat = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR,
      .pNext = &rqfeat,
   };
   VkPhysicalDeviceBufferDeviceAddressFeatures bdafeat = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES,
      .pNext = &asfeat,
   };
   VkPhysicalDeviceFeatures2 feat2 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
      .pNext = &bdafeat,
   };
   vkGetPhysicalDeviceFeatures2(pdev, &feat2);
   report(rqfeat.rayQuery, "VkPhysicalDeviceRayQueryFeaturesKHR::rayQuery = %u",
          rqfeat.rayQuery);
   if (!rqfeat.rayQuery)
      return 1;

   /* ---- 3. a device with all three enabled ---- */
   const char *devexts[] = {
      VK_KHR_ACCELERATION_STRUCTURE_EXTENSION_NAME,
      VK_KHR_DEFERRED_HOST_OPERATIONS_EXTENSION_NAME,
      VK_KHR_RAY_QUERY_EXTENSION_NAME,
   };
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &feat2,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 3,
      .ppEnabledExtensionNames = devexts,
   };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   report(1, "vkCreateDevice with ray_query enabled");

#define LOAD(var, fn) var = (PFN_vk##fn)vkGetDeviceProcAddr(dev, "vk" #fn)
   LOAD(pGetSizes, GetAccelerationStructureBuildSizesKHR);
   LOAD(pCreateAS, CreateAccelerationStructureKHR);
   LOAD(pDestroyAS, DestroyAccelerationStructureKHR);
   LOAD(pGetASAddr, GetAccelerationStructureDeviceAddressKHR);
   LOAD(pCmdBuild, CmdBuildAccelerationStructuresKHR);
#undef LOAD

   /* ---- 4. BLAS: one triangle in the z = 1 plane ---- */
   static const float verts[3][3] = {
      {0.0f, 0.0f, 1.0f}, {1.0f, 0.0f, 1.0f}, {0.0f, 1.0f, 1.0f}};
   struct buf vb = make_buf(
      sizeof(verts),
      VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR);
   memcpy(vb.map, verts, sizeof(verts));

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
   pGetSizes(dev, VK_ACCELERATION_STRUCTURE_BUILD_TYPE_DEVICE_KHR, &blas_info,
             &one, &blas_sizes);

   struct buf blas_store =
      make_buf(blas_sizes.accelerationStructureSize,
               VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR);
   struct buf blas_scratch =
      make_buf(blas_sizes.buildScratchSize ? blas_sizes.buildScratchSize : 256,
               VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);

   VkAccelerationStructureKHR blas;
   VkAccelerationStructureCreateInfoKHR blas_ci = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_CREATE_INFO_KHR,
      .buffer = blas_store.buffer,
      .size = blas_sizes.accelerationStructureSize,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL_KHR,
   };
   CHECK(pCreateAS(dev, &blas_ci, NULL, &blas));

   VkAccelerationStructureDeviceAddressInfoKHR adi = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_DEVICE_ADDRESS_INFO_KHR,
      .accelerationStructure = blas,
   };
   VkDeviceAddress blas_handle = pGetASAddr(dev, &adi);

   /* ---- 5. TLAS: one identity instance of that BLAS ---- */
   VkAccelerationStructureInstanceKHR inst_data = {
      .transform = {{{1, 0, 0, 0}, {0, 1, 0, 0}, {0, 0, 1, 0}}},
      .instanceCustomIndex = CUSTOM_INDEX,
      .mask = 0xFF,
      .flags = VK_GEOMETRY_INSTANCE_TRIANGLE_FACING_CULL_DISABLE_BIT_KHR,
      .accelerationStructureReference = blas_handle,
   };
   struct buf ib = make_buf(
      sizeof(inst_data),
      VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_BUILD_INPUT_READ_ONLY_BIT_KHR);
   memcpy(ib.map, &inst_data, sizeof(inst_data));

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
   pGetSizes(dev, VK_ACCELERATION_STRUCTURE_BUILD_TYPE_DEVICE_KHR, &tlas_info,
             &one, &tlas_sizes);

   struct buf tlas_store =
      make_buf(tlas_sizes.accelerationStructureSize,
               VK_BUFFER_USAGE_ACCELERATION_STRUCTURE_STORAGE_BIT_KHR);
   struct buf tlas_scratch =
      make_buf(tlas_sizes.buildScratchSize ? tlas_sizes.buildScratchSize : 256,
               VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);

   VkAccelerationStructureKHR tlas;
   VkAccelerationStructureCreateInfoKHR tlas_ci = {
      .sType = VK_STRUCTURE_TYPE_ACCELERATION_STRUCTURE_CREATE_INFO_KHR,
      .buffer = tlas_store.buffer,
      .size = tlas_sizes.accelerationStructureSize,
      .type = VK_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL_KHR,
   };
   CHECK(pCreateAS(dev, &tlas_ci, NULL, &tlas));

   /* ---- 6. descriptors: an acceleration structure and an SSBO ---- */
   struct buf out = make_buf(NRAYS * sizeof(struct result),
                             VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);
   memset(out.map, 0xCD, NRAYS * sizeof(struct result));

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
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));
   report(1, "descriptor set layout with an ACCELERATION_STRUCTURE binding");

   VkDescriptorPoolSize sizes[2] = {
      {VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR, 1},
      {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1},
   };
   VkDescriptorPoolCreateInfo dpci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
      .maxSets = 1,
      .poolSizeCount = 2,
      .pPoolSizes = sizes,
   };
   VkDescriptorPool dpool;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dpool));

   VkDescriptorSetAllocateInfo dsai = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
      .descriptorPool = dpool,
      .descriptorSetCount = 1,
      .pSetLayouts = &dsl,
   };
   VkDescriptorSet dset;
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &dset));

   VkWriteDescriptorSetAccelerationStructureKHR as_write = {
      .sType =
         VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET_ACCELERATION_STRUCTURE_KHR,
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
   report(1, "vkUpdateDescriptorSets wrote the TLAS into the set");

   /* ---- 7. the ray query compute pipeline ---- */
   VkShaderModuleCreateInfo smci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = sizeof(rqsmoke_spv),
      .pCode = rqsmoke_spv,
   };
   VkPipelineLayoutCreateInfo plci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .setLayoutCount = 1,
      .pSetLayouts = &dsl,
   };
   VkPipelineLayout playout;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &playout));

   VkComputePipelineCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .pNext = &smci,   /* VK_KHR_maintenance5: inline module, no VkShaderModule */
         .stage = VK_SHADER_STAGE_COMPUTE_BIT,
         .pName = "main",
      },
      .layout = playout,
   };
   VkPipeline pipe = VK_NULL_HANDLE;
   VkResult pr = vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL,
                                          &pipe);
   if (pr != VK_SUCCESS) {
      /* No maintenance5 inline path, or it refused: fall back to a real module. */
      VkShaderModule mod;
      CHECK(vkCreateShaderModule(dev, &smci, NULL, &mod));
      cpci.stage.pNext = NULL;
      cpci.stage.module = mod;
      pr = vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe);
   }
   report(pr == VK_SUCCESS,
          "compute pipeline from SPIR-V with OpRayQueryInitializeKHR (-> %d)",
          pr);
   if (pr != VK_SUCCESS)
      return 1;

   /* ---- 8. build the structures, then trace ---- */
   VkQueue queue;
   vkGetDeviceQueue(dev, 0, 0, &queue);

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = 0,
   };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));

   VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cmd));

   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cmd, &bi));

   blas_info.dstAccelerationStructure = blas;
   blas_info.scratchData.deviceAddress = blas_scratch.addr;
   VkAccelerationStructureBuildRangeInfoKHR blas_range = {.primitiveCount = 1};
   const VkAccelerationStructureBuildRangeInfoKHR *blas_ranges = &blas_range;
   pCmdBuild(cmd, 1, &blas_info, &blas_ranges);

   VkMemoryBarrier mb = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_WRITE_BIT_KHR,
      .dstAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_READ_BIT_KHR,
   };
   vkCmdPipelineBarrier(cmd,
                        VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR,
                        VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR,
                        0, 1, &mb, 0, NULL, 0, NULL);

   tlas_info.dstAccelerationStructure = tlas;
   tlas_info.scratchData.deviceAddress = tlas_scratch.addr;
   VkAccelerationStructureBuildRangeInfoKHR tlas_range = {.primitiveCount = 1};
   const VkAccelerationStructureBuildRangeInfoKHR *tlas_ranges = &tlas_range;
   pCmdBuild(cmd, 1, &tlas_info, &tlas_ranges);

   VkMemoryBarrier mb2 = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_WRITE_BIT_KHR,
      .dstAccessMask = VK_ACCESS_ACCELERATION_STRUCTURE_READ_BIT_KHR |
                       VK_ACCESS_SHADER_READ_BIT,
   };
   vkCmdPipelineBarrier(cmd,
                        VK_PIPELINE_STAGE_ACCELERATION_STRUCTURE_BUILD_BIT_KHR,
                        VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 1, &mb2, 0,
                        NULL, 0, NULL);

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
                        VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb3, 0, NULL, 0,
                        NULL);

   CHECK(vkEndCommandBuffer(cmd));

   VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, fence));
   VkResult wr = vkWaitForFences(dev, 1, &fence, VK_TRUE, 10ull * 1000000000ull);
   report(wr == VK_SUCCESS, "build + ray query dispatch completed (wait -> %d)",
          wr);
   if (wr != VK_SUCCESS)
      return 1;

   /* ---- 9. the answer ---- */
   const struct result *r = out.map;
   for (int i = 0; i < NRAYS; i++) {
      printf("  ray %d: type=%u t=%.4f prim=%u geom=%u inst=%u custom=0x%x "
             "bary=(%.4f,%.4f) ff=%u cands=%u tmin=%.4f flags=0x%x\n",
             i, r[i].committed_type, r[i].t, r[i].prim, r[i].geom,
             r[i].instance_id, r[i].custom_index, r[i].bary_x, r[i].bary_y,
             r[i].front_face, r[i].candidates_seen, r[i].ray_tmin,
             r[i].ray_flags);
   }

   report(r[0].committed_type == 1,
          "ray 0 committed a TRIANGLE intersection (type=%u, want 1)",
          r[0].committed_type);
   report(near(r[0].t, 1.0f), "ray 0 hit distance t = %.6f (want 1.0)", r[0].t);
   report(r[0].prim == 0 && r[0].geom == 0 && r[0].instance_id == 0,
          "ray 0 primitive/geometry/instance index = %u/%u/%u (want 0/0/0)",
          r[0].prim, r[0].geom, r[0].instance_id);
   report(r[0].custom_index == CUSTOM_INDEX,
          "ray 0 instanceCustomIndex = 0x%x (want 0x%x)", r[0].custom_index,
          CUSTOM_INDEX);
   report(near(r[0].bary_x, 0.25f) && near(r[0].bary_y, 0.25f),
          "ray 0 barycentrics = (%.6f, %.6f) (want 0.25, 0.25)", r[0].bary_x,
          r[0].bary_y);
   report(near(r[0].ray_tmin, 0.001f), "ray 0 rayQueryGetRayTMin = %.6f (want 0.001)",
          r[0].ray_tmin);
   report(r[0].ray_flags == 1u,
          "ray 0 rayQueryGetRayFlags = 0x%x (want 0x1, gl_RayFlagsOpaque)",
          r[0].ray_flags);

   int misses = 0;
   for (int i = 1; i < NRAYS; i++)
      misses += (r[i].committed_type == 0);
   report(misses == NRAYS - 1, "rays 1..%d all missed (%d of %d)", NRAYS - 1,
          misses, NRAYS - 1);

   /* Cleanup enough to catch a teardown crash. */
   vkDeviceWaitIdle(dev);
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyPipeline(dev, pipe, NULL);
   vkDestroyPipelineLayout(dev, playout, NULL);
   vkDestroyDescriptorPool(dev, dpool, NULL);
   vkDestroyDescriptorSetLayout(dev, dsl, NULL);
   pDestroyAS(dev, tlas, NULL);
   pDestroyAS(dev, blas, NULL);

   printf("\n%d/%d checks passed\n", pass_count, pass_count + fail_count);

   /* Say which half failed rather than leaving a bare count. The hit path and
    * everything else fail for completely different reasons, and conflating them
    * is how "ray tracing works" gets claimed off a harness that only proved a
    * pipeline compiles. */
   if (fail_count == 0) {
      printf("VERDICT: ray query works — real hit, real miss.\n");
   } else if (r[0].committed_type == 0 && misses == NRAYS - 1) {
      printf(
         "VERDICT: every ray missed. The shader path is exercised (pipeline,\n"
         "         descriptors, dispatch, flags/tmin accessors, miss path) but\n"
         "         nothing intersects. On Apple8 and earlier that is expected:\n"
         "         this GPU has no Metal 4 ray tracing, so the MTL4 acceleration\n"
         "         structure build produces a structure that traces to nothing.\n"
         "         Confirm with mtl4-rt-matrix before blaming the driver.\n");
   } else {
      printf("VERDICT: wrong intersection results — this is a driver bug, not\n"
             "         the Metal 4 build limitation (some rays did hit).\n");
   }
   return fail_count ? 1 : 0;
}
