/* gs-smoke.c — does a Vulkan geometry shader actually render on KosmicKrisp?
 *
 * Metal has no geometry stage. KosmicKrisp emulates one the way every other
 * Metal/D3D-era translation layer does: the vertex shader is turned into a
 * compute prepass that writes its outputs to a buffer, and libpoly
 * (src/poly/nir/poly_nir_lower_gs.c, shared with Honeykrisp) rewrites the
 * geometry shader into a *vertex* shader that re-runs the geometry body once per
 * output vertex and selects the one the rasterizer asked for. The draw that
 * follows is an ordinary Metal draw whose vertex/instance counts encode
 * (input primitive, output vertex).
 *
 * That means a rendered triangle is not much evidence on its own: the failure
 * modes are all "geometry appears, but attached to the wrong input primitive or
 * the wrong instance". So every check here reads back real pixels and checks a
 * value that can only be right if the ID inversion is right:
 *
 *   - the RED channel carries a per-input-VERTEX value from the vertex shader,
 *     so a geometry shader reading gl_in[] from the wrong slot fails.
 *   - the BLUE channel carries gl_PrimitiveIDIn, so a wrong instance-ID -> input
 *     primitive inversion fails even when the geometry is in the right place.
 *   - the gap between the two disjoint triangles of the list case is checked to
 *     be background, so a driver that drew the declared triangle_strip instead of
 *     the pattern-matched triangle list fails.
 *   - the layer case renders to a 2-layer array image and requires each layer to
 *     hold only the primitives that selected it.
 *
 * Case 0 is the CONTROL and the most important one: a plain VS+FS draw with no
 * geometry shader anywhere. The geometry work edits shared draw code, so a
 * regression there breaks every ordinary draw while leaving the geometry checks
 * green. Read case 0 first.
 *
 * Cases 1-3 must pass. Case 4 must be REFUSED: its output vertex count is data
 * dependent, so libpoly emits an index buffer of strips separated by a restart
 * index and Metal has no primitive restart. Until that unroll lands, refusing
 * the pipeline is the correct behaviour and rendering anything is a bug.
 *
 * REQUIRES MESA_KK_DEBUG=gs. The geometryShader feature bit is opt-in precisely
 * because cases like #4 are not implemented; run-gs-smoke.sh sets it.
 */
#include <vulkan/vulkan.h>

#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Written by run-gs-smoke.sh; one `const uint32_t <name>[]` per shader. */
#include "gs_smoke_spv.h"

#define W 64
#define H 64
#define NPOINTS 4

static VkInstance inst;
static VkPhysicalDevice pdev;
static VkDevice dev;
static VkQueue queue;
static VkCommandPool pool;
static VkPhysicalDeviceMemoryProperties memprops;

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
   uint32_t layers;
};

static void
target_init(struct target *t, uint32_t layers)
{
   t->layers = layers;

   VkImageCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = {W, H, 1},
      .mipLevels = 1,
      .arrayLayers = layers,
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
      .viewType = layers > 1 ? VK_IMAGE_VIEW_TYPE_2D_ARRAY
                             : VK_IMAGE_VIEW_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, layers},
   };
   CHECK(vkCreateImageView(dev, &vci, NULL, &t->view));

   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = (VkDeviceSize)W * H * 4 * layers,
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
}

static void
target_fini(struct target *t)
{
   vkDestroyImageView(dev, t->view, NULL);
   vkDestroyImage(dev, t->image, NULL);
   vkFreeMemory(dev, t->mem, NULL);
   vkDestroyBuffer(dev, t->readback, NULL);
   vkFreeMemory(dev, t->readback_mem, NULL);
}

/* Build a VS+GS+FS pipeline for dynamic rendering. Returns VK_SUCCESS or the
 * driver's error; the caller decides whether an error is a failure. */
static VkResult
make_pipeline(VkShaderModule vs, VkShaderModule gs, VkShaderModule fs,
              uint32_t view_layers, VkPipelineLayout layout,
              VkPipeline *pipeline_out)
{
   /* gs == VK_NULL_HANDLE builds a plain VS+FS pipeline: the control case. */
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
      .topology = gs == VK_NULL_HANDLE ? VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST
                                       : VK_PRIMITIVE_TOPOLOGY_POINT_LIST,
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
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      /* No culling: the strip case emits vertices in an order whose winding is
       * not the point of the test, and culling would hide a real failure behind
       * a plausible one. */
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
      .layout = layout,
   };
   (void)view_layers;

   return vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL,
                                    pipeline_out);
}

/* Record + submit + wait; leaves the rendered layers in t->readback. */
static void
render(struct target *t, VkPipeline pipeline, uint32_t vertex_count)
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
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, t->layers},
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
      .layerCount = t->layers,
      .colorAttachmentCount = 1,
      .pColorAttachments = &att,
   };
   vkCmdBeginRendering(cb, &ri);
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
   vkCmdDraw(cb, vertex_count, 1, 0, 0);
   vkCmdEndRendering(cb);

   VkImageMemoryBarrier to_src = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
      .image = t->image,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, t->layers},
   };
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &to_src);

   VkBufferImageCopy copy = {
      .bufferOffset = 0,
      .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, t->layers},
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
px(const uint8_t *base, uint32_t layer, int x, int y)
{
   return base + ((size_t)layer * W * H + (size_t)y * W + x) * 4;
}

/* Non-background pixels in a layer. Printed rather than asserted: the useful
 * thing about it is telling "nothing rendered" apart from "rendered in the wrong
 * place", which is the difference between a broken dispatch and a broken ID
 * inversion. */
static int
coverage(const uint8_t *base, uint32_t layer)
{
   int n = 0;
   for (int y = 0; y < H; ++y)
      for (int x = 0; x < W; ++x)
         if (px(base, layer, x, y)[1] != 0)
            n++;
   return n;
}

/* Centre of the quad the geometry shader should have grown around input point i,
 * in pixels. The vertex shader places point i at NDC x = (i + 0.5)/4 * 2 - 1,
 * y = 0. Vulkan NDC y points down the framebuffer. */
static void
point_centre(int i, int *cx, int *cy)
{
   float ndc_x = ((float)i + 0.5f) / (float)NPOINTS * 2.0f - 1.0f;
   *cx = (int)((ndc_x * 0.5f + 0.5f) * W);
   *cy = H / 2;
}

int
main(void)
{
   /* ------------------------------------------------------------- instance */
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "gs-smoke",
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
      printf("FATAL no physical devices\n");
      return 2;
   }
   VkPhysicalDevice devs[8];
   n = n > 8 ? 8 : n;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, devs));
   pdev = devs[0];

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);
   printf("device: %s\n", props.deviceName);

   VkPhysicalDeviceFeatures feats;
   vkGetPhysicalDeviceFeatures(pdev, &feats);
   check(feats.geometryShader,
         "geometryShader advertised (needs MESA_KK_DEBUG=gs)");
   if (!feats.geometryShader) {
      printf("\n%d/%d — cannot continue without the feature bit\n", passed,
             passed + failed);
      return 1;
   }
   printf("limits: maxGeometryOutputVertices=%u maxGeometryOutputComponents=%u "
          "maxGeometryShaderInvocations=%u\n",
          props.limits.maxGeometryOutputVertices,
          props.limits.maxGeometryOutputComponents,
          props.limits.maxGeometryShaderInvocations);

   vkGetPhysicalDeviceMemoryProperties(pdev, &memprops);

   /* --------------------------------------------------------------- device */
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkPhysicalDeviceFeatures want = {.geometryShader = VK_TRUE};
   VkPhysicalDeviceVulkan13Features f13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .dynamicRendering = VK_TRUE,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &f13,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .pEnabledFeatures = &want,
   };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, 0, 0, &queue);

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = 0,
   };
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));

   VkPipelineLayoutCreateInfo plci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
   };
   VkPipelineLayout layout;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &layout));

   VkShaderModule vs = module_from(gs_smoke_vert, sizeof(gs_smoke_vert));
   VkShaderModule vs_plain =
      module_from(gs_smoke_plain_vert, sizeof(gs_smoke_plain_vert));
   VkShaderModule fs = module_from(gs_smoke_frag, sizeof(gs_smoke_frag));
   VkShaderModule gs_strip =
      module_from(gs_smoke_strip_geom, sizeof(gs_smoke_strip_geom));
   VkShaderModule gs_list =
      module_from(gs_smoke_list_geom, sizeof(gs_smoke_list_geom));
   VkShaderModule gs_layer =
      module_from(gs_smoke_layer_geom, sizeof(gs_smoke_layer_geom));
   VkShaderModule gs_dyn =
      module_from(gs_smoke_dyn_geom, sizeof(gs_smoke_dyn_geom));

   /* ============================================ 0. control: no GS at all === */
   printf("\n0. control: plain VS+FS, no geometry shader\n");
   {
      VkPipeline p;
      VkResult r = make_pipeline(vs_plain, VK_NULL_HANDLE, fs, 1, layout, &p);
      check(r == VK_SUCCESS, "pipeline created (%d)", r);
      if (r == VK_SUCCESS) {
         struct target t;
         target_init(&t, 1);
         render(&t, p, 3);

         void *map;
         CHECK(vkMapMemory(dev, t.readback_mem, 0, VK_WHOLE_SIZE, 0, &map));
         const uint8_t *img = map;
         int cov = coverage(img, 0);
         printf("     coverage: %d px\n", cov);
         /* The triangle spans 1.0 x 1.0 of NDC by 0.5 of height, i.e. about
          * 1/8 of a 64x64 target. Any figure in the hundreds means the ordinary
          * hardware vertex path still renders; 0 means the geometry-shader work
          * broke every draw in the driver. */
         check(cov > 400 && cov < 800, "ordinary draws still render (%d px)",
               cov);
         vkUnmapMemory(dev, t.readback_mem);
         target_fini(&t);
         vkDestroyPipeline(dev, p, NULL);
      }
   }

   /* ================================================ 1. strip (PER_PRIM) === */
   printf("\n1. points -> triangle_strip x4  (expect POLY_GS_SHAPE_STATIC_PER_PRIM)\n");
   {
      VkPipeline p;
      VkResult r = make_pipeline(vs, gs_strip, fs, 1, layout, &p);
      check(r == VK_SUCCESS, "pipeline created (%d)", r);
      if (r == VK_SUCCESS) {
         struct target t;
         target_init(&t, 1);
         render(&t, p, NPOINTS);

         void *map;
         CHECK(vkMapMemory(dev, t.readback_mem, 0, VK_WHOLE_SIZE, 0, &map));
         const uint8_t *img = map;

         printf("     coverage: %d px\n", coverage(img, 0));

         int expanded = 0, tinted = 0, primided = 0;
         for (int i = 0; i < NPOINTS; ++i) {
            int cx, cy;
            point_centre(i, &cx, &cy);
            const uint8_t *p0 = px(img, 0, cx, cy);
            /* A quad grew here at all. */
            if (p0[1] != 0)
               expanded++;
            /* red == per-input-vertex value forwarded through the GS. */
            uint8_t want_r = (uint8_t)((i + 1) * 255 / NPOINTS);
            if (abs((int)p0[0] - (int)want_r) <= 2)
               tinted++;
            /* blue == gl_PrimitiveIDIn + 1, scaled. */
            uint8_t want_b = (uint8_t)((i + 1) * 255 / NPOINTS);
            if (abs((int)p0[2] - (int)want_b) <= 2)
               primided++;
         }
         check(expanded == NPOINTS, "all %d points expanded to geometry (%d)",
               NPOINTS, expanded);
         check(tinted == NPOINTS,
               "per-input-vertex value survived VS->GS (%d/%d)", tinted,
               NPOINTS);
         check(primided == NPOINTS, "gl_PrimitiveIDIn correct (%d/%d)",
               primided, NPOINTS);

         /* Outside every quad must still be background: an expansion that
          * covered the whole screen would pass every check above. */
         const uint8_t *corner = px(img, 0, 1, 1);
         check(corner[0] == 0 && corner[1] == 0 && corner[2] == 0,
               "background outside the quads is untouched");

         vkUnmapMemory(dev, t.readback_mem);
         target_fini(&t);
         vkDestroyPipeline(dev, p, NULL);
      }
   }

   /* ============================================ 2. list (PER_INSTANCE) === */
   printf("\n2. points -> two disjoint triangles  (expect "
          "POLY_GS_SHAPE_STATIC_PER_INSTANCE, strip rewritten to list)\n");
   {
      VkPipeline p;
      VkResult r = make_pipeline(vs, gs_list, fs, 1, layout, &p);
      check(r == VK_SUCCESS, "pipeline created (%d)", r);
      if (r == VK_SUCCESS) {
         struct target t;
         target_init(&t, 1);
         render(&t, p, NPOINTS);

         void *map;
         CHECK(vkMapMemory(dev, t.readback_mem, 0, VK_WHOLE_SIZE, 0, &map));
         const uint8_t *img = map;

         printf("     coverage: %d px\n", coverage(img, 0));

         int first = 0, second = 0, gap_clean = 0;
         for (int i = 0; i < NPOINTS; ++i) {
            int cx, cy;
            point_centre(i, &cx, &cy);
            /* The first triangle is the half of the box above-left of the
             * anti-diagonal through the point; the second is the half
             * below-right. h is 0.24 NDC, i.e. 7.7 px, so +-5 px on both axes is
             * comfortably inside each and outside the other. */
            if (px(img, 0, cx - 5, cy - 5)[1] != 0)
               first++;
            if (px(img, 0, cx + 5, cy + 5)[1] != 0)
               second++;
            /* The point's own centre pixel. It is in neither declared triangle.
             * But if the driver drew the *declared* triangle_strip instead of the
             * list libpoly pattern-matched it into, the stitched triangle
             * (v1,v2,v3) covers exactly this pixel — so this single pixel is the
             * strip/list discriminator. */
            if (px(img, 0, cx, cy)[1] == 0)
               gap_clean++;
         }
         check(first == NPOINTS, "first triangle of each pair drawn (%d/%d)",
               first, NPOINTS);
         check(second == NPOINTS, "second triangle of each pair drawn (%d/%d)",
               second, NPOINTS);
         check(gap_clean == NPOINTS,
               "no stitched triangle between the pair, so it was drawn as a "
               "LIST (%d/%d)",
               gap_clean, NPOINTS);

         vkUnmapMemory(dev, t.readback_mem);
         target_fini(&t);
         vkDestroyPipeline(dev, p, NULL);
      }
   }

   /* ================================================ 3. per-prim gl_Layer === */
   printf("\n3. per-primitive gl_Layer into a 2-layer array image\n");
   {
      VkPipeline p;
      VkResult r = make_pipeline(vs, gs_layer, fs, 2, layout, &p);
      check(r == VK_SUCCESS, "pipeline created (%d)", r);
      if (r == VK_SUCCESS) {
         struct target t;
         target_init(&t, 2);
         render(&t, p, NPOINTS);

         void *map;
         CHECK(vkMapMemory(dev, t.readback_mem, 0, VK_WHOLE_SIZE, 0, &map));
         const uint8_t *img = map;

         printf("     coverage: layer0 %d px, layer1 %d px\n",
                coverage(img, 0), coverage(img, 1));

         int right = 0, leaked = 0;
         for (int i = 0; i < NPOINTS; ++i) {
            int cx, cy;
            point_centre(i, &cx, &cy);
            uint32_t want_layer = (uint32_t)(i & 1);
            if (px(img, want_layer, cx, cy)[1] != 0)
               right++;
            if (px(img, 1 - want_layer, cx, cy)[1] != 0)
               leaked++;
         }
         check(right == NPOINTS, "each primitive landed on its chosen layer "
                                 "(%d/%d)", right, NPOINTS);
         check(leaked == 0,
               "no primitive leaked onto the other layer (%d leaks)", leaked);

         vkUnmapMemory(dev, t.readback_mem);
         target_fini(&t);
         vkDestroyPipeline(dev, p, NULL);
      }
   }

   /* ========================================= 4. dynamic topology: refuse === */
   printf("\n4. data-dependent vertex count  (expect a REFUSAL, not a render)\n");
   {
      VkPipeline p = VK_NULL_HANDLE;
      VkResult r = make_pipeline(vs, gs_dyn, fs, 1, layout, &p);
      check(r != VK_SUCCESS,
            "pipeline refused, since Metal has no primitive restart (%d)", r);
      if (r == VK_SUCCESS)
         vkDestroyPipeline(dev, p, NULL);
   }

   printf("\n%d/%d\n", passed, passed + failed);
   return failed ? 1 : 0;
}
