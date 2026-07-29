/* pv-smoke.c — does VK_EXT_provoking_vertex select the right vertex on
 * KosmicKrisp, in BOTH modes, on every path that produces a primitive?
 *
 * Metal's provoking vertex is the FIRST vertex of a primitive and there is no
 * state anywhere in Metal to change it. Vulkan's VK_PROVOKING_VERTEX_MODE_LAST_
 * VERTEX_EXT — the mode ANGLE requests on every pipeline it builds, and the one
 * whose absence pins the guest's GLES to 2.0 — asks for the other one. So the
 * driver emulates it: it decomposes the draw into independent primitives in the
 * last-vertex vertex order and rotates each one so the provoking vertex lands in
 * the slot Metal reads.
 *
 * A TRIANGLE STRIP is where that actually matters, and it is the shape this test
 * is built around. For a strip of six vertices the four triangles' provoking
 * vertices are 0,1,2,3 in first-vertex mode and 2,3,4,5 in last-vertex mode —
 * disjoint sets, so no check here can pass by accident or pass in the wrong
 * mode. Every check reads back the actual pixel and reports the vertex index
 * that won, per primitive.
 *
 * What each group is for:
 *
 *   0  The extension is advertised and provokingVertexLast is VK_TRUE. Before
 *      this work the whole test was UNRUNNABLE: vkCreateDevice with
 *      VK_EXT_provoking_vertex failed, which is the "fails before" evidence.
 *   1  Triangle strip, both modes. The core claim.
 *   2  The two modes' COVERAGE is pixel-identical. The rotation is cyclic, so it
 *      must move no geometry — this fails if the emulation ever swapped two
 *      vertices (which would flip the winding) instead of rotating three.
 *   3  Triangle list, line strip, triangle fan, and an INDEXED strip. Each is a
 *      separate producer inside the driver: the list has no decomposition to do
 *      but still needs the rotation, the fan has its own vertex-order rule, the
 *      line topologies are two-vertex primitives where the rotation is a swap,
 *      and the indexed path is the one that reads the application's index buffer
 *      rather than synthesising one.
 *   4  Two pipelines with DIFFERENT modes inside ONE render pass. This is
 *      exactly what provokingVertexModePerPipeline == VK_TRUE promises, and it
 *      is the property that would break if the mode were baked into a shader
 *      rather than read from state at draw time.
 *   5  A pipeline with no flat input at all, rendered in both modes, compared
 *      byte for byte. The driver deliberately skips all of the above when
 *      nothing can observe the provoking vertex; this is the check that the skip
 *      is invisible.
 *   6  Geometry shaders, one case per output shape libpoly can pick. The
 *      geometry shader is the subtle one: its output topology is decomposed by
 *      four different producers (a CPU unroll of a constant index buffer, a
 *      synthesised index buffer for each of the two non-indexed shapes, and a
 *      GPU kernel for the dynamic one) and all four have to agree with the
 *      rasterizer AND with the input assembly, which reads the mode out of the
 *      root table. Requires MESA_KK_DEBUG=gs, which run-pv-smoke.sh sets.
 */
#include <vulkan/vulkan.h>

#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Written by run-pv-smoke.sh; one `const uint32_t <name>[]` per shader. */
#include "pv_smoke_spv.h"

#define W 64
#define H 64

/* pv-smoke.vert's push constant. */
#define LAYOUT_RIBBON 0u
#define LAYOUT_LINE 1u
#define LAYOUT_FAN 2u

static VkInstance inst;
static VkPhysicalDevice pdev;
static VkDevice dev;
static VkQueue queue;
static VkCommandPool pool;
static VkPhysicalDeviceMemoryProperties memprops;
static VkPipelineLayout pipeline_layout;
static bool have_gs;

static int passed, failed;

#define CHECK(x)                                                               \
   do {                                                                        \
      VkResult _r = (x);                                                       \
      if (_r != VK_SUCCESS) {                                                  \
         printf("FATAL %s -> %d\n", #x, _r);                                   \
         exit(2);                                                              \
      }                                                                        \
   } while (0)

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
      passed++;
   else
      failed++;
}

static uint32_t
find_mem(uint32_t type_bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; ++i) {
      if ((type_bits & (1u << i)) &&
          (memprops.memoryTypes[i].propertyFlags & want) == want)
         return i;
   }
   printf("FATAL no memory type for %x/%x\n", type_bits, want);
   exit(2);
}

static VkShaderModule
module_from(const uint32_t *code, size_t size_B)
{
   VkShaderModuleCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = size_B,
      .pCode = code,
   };
   VkShaderModule m;
   CHECK(vkCreateShaderModule(dev, &ci, NULL, &m));
   return m;
}

/* ---------------------------------------------------------------- rendering */

struct target {
   VkImage image;
   VkDeviceMemory mem;
   VkImageView view;
   VkBuffer readback;
   VkDeviceMemory readback_mem;
   uint8_t *pixels;
};

static void
target_init(struct target *t)
{
   VkImageCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = {W, H, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
               VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   CHECK(vkCreateImage(dev, &ici, NULL, &t->image));

   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, t->image, &mr);
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex =
         find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
   };
   CHECK(vkAllocateMemory(dev, &ai, NULL, &t->mem));
   CHECK(vkBindImageMemory(dev, t->image, t->mem, 0));

   VkImageViewCreateInfo vci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = t->image,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   CHECK(vkCreateImageView(dev, &vci, NULL, &t->view));

   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = (VkDeviceSize)W * H * 4,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
   };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &t->readback));
   vkGetBufferMemoryRequirements(dev, t->readback, &mr);
   ai.allocationSize = mr.size;
   ai.memoryTypeIndex = find_mem(mr.memoryTypeBits,
                                 VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                    VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
   CHECK(vkAllocateMemory(dev, &ai, NULL, &t->readback_mem));
   CHECK(vkBindBufferMemory(dev, t->readback, t->readback_mem, 0));
   CHECK(vkMapMemory(dev, t->readback_mem, 0, VK_WHOLE_SIZE, 0,
                     (void **)&t->pixels));
}

static void
target_fini(struct target *t)
{
   vkUnmapMemory(dev, t->readback_mem);
   vkDestroyImageView(dev, t->view, NULL);
   vkDestroyImage(dev, t->image, NULL);
   vkFreeMemory(dev, t->mem, NULL);
   vkDestroyBuffer(dev, t->readback, NULL);
   vkFreeMemory(dev, t->readback_mem, NULL);
}

/* Build a VS(+GS)+FS pipeline. `mode` is chained onto the rasterization state
 * exactly the way ANGLE chains it, which is the only way this driver is asked
 * for the extension in practice. */
static VkResult
make_pipeline(VkShaderModule vs, VkShaderModule gs, VkShaderModule fs,
              VkPrimitiveTopology topology, VkProvokingVertexModeEXT mode,
              VkPipeline *pipeline_out)
{
   uint32_t stage_count = gs == VK_NULL_HANDLE ? 2u : 3u;
   VkPipelineShaderStageCreateInfo stages[] = {
      {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_VERTEX_BIT,
         .module = vs,
         .pName = "main",
      },
      {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = gs == VK_NULL_HANDLE ? VK_SHADER_STAGE_FRAGMENT_BIT
                                       : VK_SHADER_STAGE_GEOMETRY_BIT,
         .module = gs == VK_NULL_HANDLE ? fs : gs,
         .pName = "main",
      },
      {
         .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
         .module = fs,
         .pName = "main",
      },
   };

   VkPipelineVertexInputStateCreateInfo vi = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = topology,
   };
   VkViewport vp = {0, 0, W, H, 0, 1};
   VkRect2D sc = {{0, 0}, {W, H}};
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &sc,
   };
   VkPipelineRasterizationProvokingVertexStateCreateInfoEXT pv = {
      .sType =
         VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_PROVOKING_VERTEX_STATE_CREATE_INFO_EXT,
      .provokingVertexMode = mode,
   };
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .pNext = &pv,
      .polygonMode = VK_POLYGON_MODE_FILL,
      /* No culling. The rotation is winding-neutral and group 2 proves it by
       * comparing coverage; culling here would turn that measurement into a
       * pass/fail on geometry that never reached the rasterizer. */
      .cullMode = VK_CULL_MODE_NONE,
      .lineWidth = 1.0f,
   };
   VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
   };
   VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
   VkPipelineColorBlendStateCreateInfo cb = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &cba,
   };
   VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
   VkPipelineRenderingCreateInfo pr = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
      .colorAttachmentCount = 1,
      .pColorAttachmentFormats = &fmt,
   };
   VkGraphicsPipelineCreateInfo gpci = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .pNext = &pr,
      .stageCount = stage_count,
      .pStages = stages,
      .pVertexInputState = &vi,
      .pInputAssemblyState = &ia,
      .pViewportState = &vps,
      .pRasterizationState = &rs,
      .pMultisampleState = &ms,
      .pColorBlendState = &cb,
      .layout = pipeline_layout,
   };

   return vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL,
                                    pipeline_out);
}

/* One draw inside a render pass. Several checks need two of them back to back,
 * which is why this is a list rather than an argument set. */
struct pass_draw {
   VkPipeline pipeline;
   uint32_t vertex_count;
   uint32_t first_vertex;
   uint32_t layout_id;
   VkBuffer index_buffer; /* VK_NULL_HANDLE for a non-indexed draw */
};

static void
render(struct target *t, const struct pass_draw *draws, uint32_t draw_count)
{
   VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));

   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cb, &bi));

   VkImageMemoryBarrier to_color = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .image = t->image,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0,
                        NULL, 0, NULL, 1, &to_color);

   VkRenderingAttachmentInfo att = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = t->view,
      .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue = {.color = {.float32 = {0, 0, 0, 1}}},
   };
   VkRenderingInfo ri = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = {{0, 0}, {W, H}},
      .layerCount = 1,
      .colorAttachmentCount = 1,
      .pColorAttachments = &att,
   };
   vkCmdBeginRendering(cb, &ri);
   for (uint32_t i = 0; i < draw_count; ++i) {
      const struct pass_draw *d = &draws[i];
      vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, d->pipeline);
      vkCmdPushConstants(cb, pipeline_layout, VK_SHADER_STAGE_VERTEX_BIT, 0,
                         sizeof(uint32_t), &d->layout_id);
      if (d->index_buffer != VK_NULL_HANDLE) {
         vkCmdBindIndexBuffer(cb, d->index_buffer, 0, VK_INDEX_TYPE_UINT16);
         vkCmdDrawIndexed(cb, d->vertex_count, 1, d->first_vertex, 0, 0);
      } else {
         vkCmdDraw(cb, d->vertex_count, 1, d->first_vertex, 0);
      }
   }
   vkCmdEndRendering(cb);

   VkImageMemoryBarrier to_src = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
      .image = t->image,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &to_src);

   VkBufferImageCopy copy = {
      .bufferOffset = 0,
      .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
      .imageExtent = {W, H, 1},
   };
   vkCmdCopyImageToBuffer(cb, t->image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                          t->readback, 1, &copy);

   CHECK(vkEndCommandBuffer(cb));

   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cb,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
   CHECK(vkQueueWaitIdle(queue));
   vkFreeCommandBuffers(dev, pool, 1, &cb);
}

/* ------------------------------------------------------------------- pixels */

static const uint8_t *
px(const uint8_t *base, int x, int y)
{
   return base + ((size_t)y * W + x) * 4;
}

/* The provoking vertex index at (x, y), or -1 for background. pv-smoke.frag
 * writes vertex+1 into red and 255 into green, so green is the coverage bit and
 * red names the winner. */
static int
provoking_at(const uint8_t *base, int x, int y)
{
   const uint8_t *p = px(base, x, y);
   return p[1] == 0 ? -1 : (int)p[0] - 1;
}

/* Same, but searching a whole column for the one covered pixel. Line
 * rasterization decides for itself which row of a horizontal segment it lights;
 * the test cares which VERTEX won, not which row. */
static int
provoking_in_column(const uint8_t *base, int x)
{
   for (int y = 0; y < H; ++y) {
      if (px(base, x, y)[1] != 0)
         return (int)px(base, x, y)[0] - 1;
   }
   return -1;
}

static int
coverage(const uint8_t *base)
{
   int n = 0;
   for (int y = 0; y < H; ++y)
      for (int x = 0; x < W; ++x)
         if (px(base, x, y)[1] != 0)
            n++;
   return n;
}

/* A sample point per primitive, and the vertex expected to win there. */
struct sample {
   int x, y;
};

static void
check_provoking(const uint8_t *base, const struct sample *at,
                const int *expect, uint32_t n, bool columns,
                const char *what)
{
   char got_s[128] = {0}, want_s[128] = {0};
   uint32_t ok = 0;

   for (uint32_t i = 0; i < n; ++i) {
      int got = columns ? provoking_in_column(base, at[i].x)
                        : provoking_at(base, at[i].x, at[i].y);
      if (got == expect[i])
         ok++;
      snprintf(got_s + strlen(got_s), sizeof(got_s) - strlen(got_s), "%s%d",
               i ? "," : "", got);
      snprintf(want_s + strlen(want_s), sizeof(want_s) - strlen(want_s), "%s%d",
               i ? "," : "", expect[i]);
   }

   check(ok == n, "%s: provoking vertex per primitive = %s (want %s)", what,
         got_s, want_s);
}

/* --------------------------------------------------------------------- cases */

/* The 3x2 ribbon's four strip triangles, one interior sample each: the two
 * halves of the left square then the two halves of the right one. */
static const struct sample strip_at[4] = {
   {4, 4}, {28, 60}, {36, 4}, {60, 60}};
/* pv-smoke.vert's ribbon read as an independent triangle list: v0,v1,v2 is the
 * upper-left half of the left square and v3,v4,v5 the lower-right half of the
 * right one. */
static const struct sample list_at[2] = {{4, 4}, {60, 60}};
/* Two horizontal segments, sampled by column. */
static const struct sample line_at[2] = {{16, 0}, {48, 0}};
/* The fan's two wedges around the centre. */
static const struct sample fan_at[2] = {{32, 6}, {58, 32}};
/* A full-framebuffer quad split by its TL-BR diagonal. */
static const struct sample quad_at[2] = {{4, 4}, {60, 60}};
/* Two half-framebuffer quads, each split by its own diagonal. */
static const struct sample two_quad_at[4] = {
   {4, 4}, {28, 60}, {36, 4}, {60, 60}};

static VkPipeline
pipeline_or_die(VkShaderModule vs, VkShaderModule gs, VkShaderModule fs,
                VkPrimitiveTopology topology, VkProvokingVertexModeEXT mode,
                const char *what)
{
   VkPipeline p = VK_NULL_HANDLE;
   VkResult r = make_pipeline(vs, gs, fs, topology, mode, &p);
   if (r != VK_SUCCESS) {
      printf("FATAL %s pipeline -> %d\n", what, r);
      exit(2);
   }
   return p;
}

int
main(void)
{
   setvbuf(stdout, NULL, _IONBF, 0);

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "pv-smoke",
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t n = 0;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, NULL));
   if (n == 0) {
      printf("FATAL no physical device\n");
      return 2;
   }
   VkPhysicalDevice devs[8];
   n = n > 8 ? 8 : n;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, devs));
   pdev = devs[0];

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);
   printf("device      : %s (api %u.%u.%u)\n", props.deviceName,
          VK_VERSION_MAJOR(props.apiVersion), VK_VERSION_MINOR(props.apiVersion),
          VK_VERSION_PATCH(props.apiVersion));
   vkGetPhysicalDeviceMemoryProperties(pdev, &memprops);

   printf("\n0. VK_EXT_provoking_vertex is there at all\n");

   uint32_t ext_count = 0;
   CHECK(vkEnumerateDeviceExtensionProperties(pdev, NULL, &ext_count, NULL));
   VkExtensionProperties *exts = calloc(ext_count, sizeof(*exts));
   CHECK(vkEnumerateDeviceExtensionProperties(pdev, NULL, &ext_count, exts));
   bool advertised = false;
   for (uint32_t i = 0; i < ext_count; ++i) {
      if (!strcmp(exts[i].extensionName, VK_EXT_PROVOKING_VERTEX_EXTENSION_NAME))
         advertised = true;
   }
   free(exts);
   check(advertised, "VK_EXT_provoking_vertex is advertised");
   if (!advertised) {
      /* Everything below needs the extension enabled on the device, so there is
       * nothing further to measure. This is the state the driver was in before
       * the extension was implemented. */
      printf("\n%d/%d\n", passed, passed + failed);
      return 1;
   }

   VkPhysicalDeviceProvokingVertexFeaturesEXT pv_feat = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROVOKING_VERTEX_FEATURES_EXT,
   };
   VkPhysicalDeviceFeatures2 feat2 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
      .pNext = &pv_feat,
   };
   vkGetPhysicalDeviceFeatures2(pdev, &feat2);
   check(pv_feat.provokingVertexLast == VK_TRUE,
         "provokingVertexLast == VK_TRUE (this is the bit ANGLE gates "
         "ANGLE_FEATURE_CONDITION(provokingVertex) on)");
   have_gs = feat2.features.geometryShader == VK_TRUE;

   VkPhysicalDeviceProvokingVertexPropertiesEXT pv_props = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROVOKING_VERTEX_PROPERTIES_EXT,
   };
   VkPhysicalDeviceProperties2 props2 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
      .pNext = &pv_props,
   };
   vkGetPhysicalDeviceProperties2(pdev, &props2);
   check(pv_props.provokingVertexModePerPipeline == VK_TRUE,
         "provokingVertexModePerPipeline == VK_TRUE (group 4 holds this to it)");

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const char *dev_exts[] = {VK_EXT_PROVOKING_VERTEX_EXTENSION_NAME};
   VkPhysicalDeviceProvokingVertexFeaturesEXT enable_pv = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROVOKING_VERTEX_FEATURES_EXT,
      .provokingVertexLast = VK_TRUE,
   };
   VkPhysicalDeviceFeatures2 enable = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
      .pNext = &enable_pv,
      .features = {.geometryShader = have_gs ? VK_TRUE : VK_FALSE},
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &enable,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = dev_exts,
   };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, 0, 0, &queue);

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
   };
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));

   VkPushConstantRange pcr = {VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(uint32_t)};
   VkPipelineLayoutCreateInfo plci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .pushConstantRangeCount = 1,
      .pPushConstantRanges = &pcr,
   };
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pipeline_layout));

   VkShaderModule vs = module_from(pv_smoke_vert, sizeof(pv_smoke_vert));
   VkShaderModule point_vs =
      module_from(pv_smoke_point_vert, sizeof(pv_smoke_point_vert));
   VkShaderModule fs = module_from(pv_smoke_frag, sizeof(pv_smoke_frag));
   VkShaderModule smooth_fs =
      module_from(pv_smoke_smooth_frag, sizeof(pv_smoke_smooth_frag));

   /* An identity index buffer, so that the indexed case draws exactly the
    * picture the non-indexed one does and any difference is the driver's. */
   uint16_t idx[6] = {0, 1, 2, 3, 4, 5};
   VkBufferCreateInfo ibci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = sizeof(idx),
      .usage = VK_BUFFER_USAGE_INDEX_BUFFER_BIT,
   };
   VkBuffer ibuf;
   CHECK(vkCreateBuffer(dev, &ibci, NULL, &ibuf));
   VkMemoryRequirements imr;
   vkGetBufferMemoryRequirements(dev, ibuf, &imr);
   VkMemoryAllocateInfo iai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = imr.size,
      .memoryTypeIndex = find_mem(imr.memoryTypeBits,
                                  VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
   };
   VkDeviceMemory imem;
   CHECK(vkAllocateMemory(dev, &iai, NULL, &imem));
   CHECK(vkBindBufferMemory(dev, ibuf, imem, 0));
   void *imap;
   CHECK(vkMapMemory(dev, imem, 0, VK_WHOLE_SIZE, 0, &imap));
   memcpy(imap, idx, sizeof(idx));
   vkUnmapMemory(dev, imem);

   struct target t;
   target_init(&t);

   static uint8_t first_pixels[W * H * 4];

   /* ------------------------------------------------- 1. the triangle strip */
   printf("\n1. flat-shaded TRIANGLE STRIP, six vertices, four triangles\n");
   {
      VkPipeline p_first = pipeline_or_die(
         vs, VK_NULL_HANDLE, fs, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP,
         VK_PROVOKING_VERTEX_MODE_FIRST_VERTEX_EXT, "strip/first");
      VkPipeline p_last = pipeline_or_die(
         vs, VK_NULL_HANDLE, fs, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP,
         VK_PROVOKING_VERTEX_MODE_LAST_VERTEX_EXT, "strip/last");

      struct pass_draw d = {p_first, 6, 0, LAYOUT_RIBBON, VK_NULL_HANDLE};
      render(&t, &d, 1);
      printf("     coverage: %d px\n", coverage(t.pixels));
      check(coverage(t.pixels) > 3000, "the strip rendered at all (%d px)",
            coverage(t.pixels));
      static const int want_first[4] = {0, 1, 2, 3};
      check_provoking(t.pixels, strip_at, want_first, 4, false, "FIRST");
      memcpy(first_pixels, t.pixels, sizeof(first_pixels));

      d.pipeline = p_last;
      render(&t, &d, 1);
      static const int want_last[4] = {2, 3, 4, 5};
      check_provoking(t.pixels, strip_at, want_last, 4, false, "LAST ");

      /* ------------------------------- 2. the rotation moved no geometry */
      printf("\n2. the two modes rasterize the SAME pixels\n");
      int diff = 0;
      for (int i = 0; i < W * H; ++i) {
         if ((first_pixels[i * 4 + 1] != 0) != (t.pixels[i * 4 + 1] != 0))
            diff++;
      }
      check(diff == 0,
            "coverage is identical between the modes (%d differing px) — a "
            "swap instead of a rotation would have flipped the winding",
            diff);

      vkDestroyPipeline(dev, p_first, NULL);
      vkDestroyPipeline(dev, p_last, NULL);
   }

   /* --------------------------------------------- 3. the other topologies */
   printf("\n3. the other primitive producers\n");
   {
      struct {
         const char *name;
         VkPrimitiveTopology topology;
         uint32_t layout_id, vertices, prims;
         const struct sample *at;
         bool columns;
         int first[4], last[4];
         bool indexed;
      } cases[] = {
         {"triangle list  ", VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
          LAYOUT_RIBBON, 6, 2, list_at, false, {0, 3}, {2, 5}, false},
         {"triangle fan   ", VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN, LAYOUT_FAN, 4,
          2, fan_at, false, {1, 2}, {2, 3}, false},
         {"line strip     ", VK_PRIMITIVE_TOPOLOGY_LINE_STRIP, LAYOUT_LINE, 3,
          2, line_at, true, {0, 1}, {1, 2}, false},
         {"line list      ", VK_PRIMITIVE_TOPOLOGY_LINE_LIST, LAYOUT_LINE, 2, 1,
          line_at, true, {0}, {1}, false},
         {"indexed strip  ", VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP,
          LAYOUT_RIBBON, 6, 4, strip_at, false, {0, 1, 2, 3}, {2, 3, 4, 5},
          true},
      };

      for (uint32_t c = 0; c < sizeof(cases) / sizeof(cases[0]); ++c) {
         for (int last = 0; last < 2; ++last) {
            VkPipeline p = pipeline_or_die(
               vs, VK_NULL_HANDLE, fs, cases[c].topology,
               last ? VK_PROVOKING_VERTEX_MODE_LAST_VERTEX_EXT
                    : VK_PROVOKING_VERTEX_MODE_FIRST_VERTEX_EXT,
               cases[c].name);
            struct pass_draw d = {p, cases[c].vertices, 0,
                                  cases[c].layout_id,
                                  cases[c].indexed ? ibuf : VK_NULL_HANDLE};
            render(&t, &d, 1);

            char what[64];
            snprintf(what, sizeof(what), "%s %s", cases[c].name,
                     last ? "LAST " : "FIRST");
            check_provoking(t.pixels, cases[c].at,
                            last ? cases[c].last : cases[c].first,
                            cases[c].prims, cases[c].columns, what);
            vkDestroyPipeline(dev, p, NULL);
         }
      }
   }

   /* ------------------------ 4. two modes inside a single render pass */
   printf("\n4. both modes in ONE render pass (provokingVertexModePerPipeline)\n");
   {
      /* Two triangle-LIST draws into disjoint halves of the framebuffer: the
       * first three ribbon vertices are the left square's upper-left triangle
       * and the last three the right square's lower-right one, so one pipeline
       * per half with a different mode each. Neither pipeline is rebound
       * between them, so nothing but the pipeline can be carrying the mode. */
      VkPipeline p_first = pipeline_or_die(
         vs, VK_NULL_HANDLE, fs, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
         VK_PROVOKING_VERTEX_MODE_FIRST_VERTEX_EXT, "mixed/first");
      VkPipeline p_last = pipeline_or_die(
         vs, VK_NULL_HANDLE, fs, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
         VK_PROVOKING_VERTEX_MODE_LAST_VERTEX_EXT, "mixed/last");

      /* Two draws into DISJOINT halves, so both answers survive to the
       * readback: vertices 0..2 are the left square's upper-left triangle and
       * vertices 3..5 the right square's lower-right one. */
      struct pass_draw both[2] = {
         {p_first, 3, 0, LAYOUT_RIBBON, VK_NULL_HANDLE},
         {p_last, 3, 3, LAYOUT_RIBBON, VK_NULL_HANDLE},
      };
      render(&t, both, 2);
      static const int want[2] = {0, 5};
      check_provoking(t.pixels, list_at, want, 2, false,
                      "FIRST then LAST in one pass");

      /* And the other way round, so neither ordering can be the one that
       * happens to work because the state was never re-emitted. */
      struct pass_draw swapped[2] = {
         {p_last, 3, 0, LAYOUT_RIBBON, VK_NULL_HANDLE},
         {p_first, 3, 3, LAYOUT_RIBBON, VK_NULL_HANDLE},
      };
      render(&t, swapped, 2);
      static const int want_swapped[2] = {2, 3};
      check_provoking(t.pixels, list_at, want_swapped, 2, false,
                      "LAST then FIRST in one pass");

      vkDestroyPipeline(dev, p_first, NULL);
      vkDestroyPipeline(dev, p_last, NULL);
   }

   /* ------------------------------- 5. nothing flat: the skip is invisible */
   printf("\n5. a pipeline with no flat input renders identically in both modes\n");
   {
      VkPipeline p_first = pipeline_or_die(
         vs, VK_NULL_HANDLE, smooth_fs, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP,
         VK_PROVOKING_VERTEX_MODE_FIRST_VERTEX_EXT, "smooth/first");
      VkPipeline p_last = pipeline_or_die(
         vs, VK_NULL_HANDLE, smooth_fs, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP,
         VK_PROVOKING_VERTEX_MODE_LAST_VERTEX_EXT, "smooth/last");

      struct pass_draw d = {p_first, 6, 0, LAYOUT_RIBBON, VK_NULL_HANDLE};
      render(&t, &d, 1);
      memcpy(first_pixels, t.pixels, sizeof(first_pixels));

      d.pipeline = p_last;
      render(&t, &d, 1);
      check(memcmp(first_pixels, t.pixels, sizeof(first_pixels)) == 0,
            "byte-identical images with a smooth-only fragment shader");

      vkDestroyPipeline(dev, p_first, NULL);
      vkDestroyPipeline(dev, p_last, NULL);
   }

   /* ------------------------------------------------- 6. geometry shaders */
   printf("\n6. geometry shader output, one case per libpoly output shape\n");
   if (!have_gs) {
      check(false, "geometryShader is not enabled — the geometry cases are the "
                   "subtlest half of this test, so a run without them is a "
                   "failed run. Use run-pv-smoke.sh, which sets MESA_KK_DEBUG=gs");
   } else {
      VkShaderModule quad_gs =
         module_from(pv_smoke_quad_geom, sizeof(pv_smoke_quad_geom));
      VkShaderModule tri_gs =
         module_from(pv_smoke_tri_geom, sizeof(pv_smoke_tri_geom));
      VkShaderModule two_gs =
         module_from(pv_smoke_2strip_geom, sizeof(pv_smoke_2strip_geom));
      VkShaderModule dyn_gs =
         module_from(pv_smoke_dyn_geom, sizeof(pv_smoke_dyn_geom));

      struct {
         const char *name;
         VkShaderModule gs;
         uint32_t prims;
         const struct sample *at;
         int first[4], last[4];
      } cases[] = {
         {"4-vertex strip (STATIC_PER_PRIM)    ", quad_gs, 2, quad_at,
          {0, 1}, {2, 3}},
         {"3-vertex strip (STATIC_PER_INSTANCE)", tri_gs, 1, quad_at, {0}, {2}},
         {"two strips     (STATIC_INDEXED)     ", two_gs, 4, two_quad_at,
          {0, 1, 4, 5}, {2, 3, 6, 7}},
         {"dynamic count  (DYNAMIC_INDEXED)    ", dyn_gs, 2, quad_at, {0, 1},
          {2, 3}},
      };

      for (uint32_t c = 0; c < sizeof(cases) / sizeof(cases[0]); ++c) {
         for (int last = 0; last < 2; ++last) {
            VkPipeline p = pipeline_or_die(
               point_vs, cases[c].gs, fs, VK_PRIMITIVE_TOPOLOGY_POINT_LIST,
               last ? VK_PROVOKING_VERTEX_MODE_LAST_VERTEX_EXT
                    : VK_PROVOKING_VERTEX_MODE_FIRST_VERTEX_EXT,
               cases[c].name);
            struct pass_draw d = {p, 1, 0, LAYOUT_RIBBON, VK_NULL_HANDLE};
            render(&t, &d, 1);

            char what[96];
            snprintf(what, sizeof(what), "%s %s", cases[c].name,
                     last ? "LAST " : "FIRST");
            check_provoking(t.pixels, cases[c].at,
                            last ? cases[c].last : cases[c].first,
                            cases[c].prims, false, what);
            vkDestroyPipeline(dev, p, NULL);
         }
      }

      vkDestroyShaderModule(dev, quad_gs, NULL);
      vkDestroyShaderModule(dev, tri_gs, NULL);
      vkDestroyShaderModule(dev, two_gs, NULL);
      vkDestroyShaderModule(dev, dyn_gs, NULL);
   }

   target_fini(&t);
   vkDestroyBuffer(dev, ibuf, NULL);
   vkFreeMemory(dev, imem, NULL);
   vkDestroyShaderModule(dev, vs, NULL);
   vkDestroyShaderModule(dev, point_vs, NULL);
   vkDestroyShaderModule(dev, fs, NULL);
   vkDestroyShaderModule(dev, smooth_fs, NULL);
   vkDestroyPipelineLayout(dev, pipeline_layout, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);

   printf("\n%d/%d\n", passed, passed + failed);
   return failed ? 1 : 0;
}
