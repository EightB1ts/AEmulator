/* sparse-smoke.c — does sparse binding work through KosmicKrisp, end to end?
 *
 * mtl4-sparse-matrix.m proves Metal placement sparse works on this GPU. This
 * proves the DRIVER exposes it correctly: it goes in through the Vulkan API only
 * and checks the things a driver can get wrong on its own — the advertised
 * features, the queue family bit, the memory requirements, the sparse image
 * granularity and mip tail, vkQueueBindSparse itself, and then whether the bytes
 * actually land where the binding says they should.
 *
 * The load-bearing checks are the ones about DATA:
 *
 *   - a buffer bound from TWO separate VkDeviceMemory objects reads back the
 *     right values in both halves. One MTLBuffer cannot span two MTLHeaps, so
 *     this is the check that says placement sparse is really being used and not
 *     faked by a whole-resource allocation.
 *   - a deliberately unbacked hole reads as zero, which is what
 *     sparseResidencyNonResidentStrict promises.
 *   - rebinding a range to a different memory makes the buffer see the other
 *     allocation's contents, so bindings are not one-shot.
 *   - the same for a sparse IMAGE, bound per tile through
 *     VkSparseImageMemoryBindInfo with its mip tail bound opaquely.
 *
 * No shaders: everything is done with vkCmdFillBuffer / vkCmdCopyBuffer /
 * vkCmdClearColorImage / vkCmdCopyImageToBuffer, so a failure here is about
 * sparse binding and not about the compiler.
 *
 * EXPECT 32/35, and expect the three failures to be exactly the UNBIND checks:
 * one in test_buffer and two in test_unmap_minimal. Unbinding a range (a
 * VkSparseMemoryBind with memory == VK_NULL_HANDLE) does not make it read as
 * non-resident; it keeps its previous contents. kk_sparse.c's header documents
 * the whole evidence trail, including the standalone Metal probes that show the
 * very same Unmap operation working outside the driver. It is left FAILING rather
 * than hidden or downgraded, because the day it starts passing is worth noticing.
 *
 * Any OTHER failure is a real regression. In particular a failure among:
 *   "backed blocks read back their fill, across BOTH allocations"  — one sparse
 *      resource drawing pages from two VkDeviceMemory, which is the whole point;
 *   "rebinding block 0 to other memory takes effect"               — remapping;
 *   "bound tile holds the clear colour" / "unbound tile reads zero" — images;
 *   "after an opaque whole-image bind ... backed"                   — the opaque
 *      tile-order translation in kk_sparse_image_geom.
 */
#include <vulkan/vulkan.h>

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static VkInstance inst;
static VkPhysicalDevice pdev;
static VkDevice dev;
static VkQueue queue;
static uint32_t qfam = UINT32_MAX;
static VkPhysicalDeviceMemoryProperties memprops;
static VkCommandPool pool;
static uint64_t block_B;

static int passes, fails;

#define CHECK(x)                                                               \
   do {                                                                        \
      VkResult _r = (x);                                                       \
      if (_r != VK_SUCCESS) {                                                  \
         printf("FATAL %s -> %d\n", #x, _r);                                   \
         exit(1);                                                              \
      }                                                                        \
   } while (0)

static void
ok(int cond, const char *fmt, ...)
{
   va_list ap;
   if (cond)
      passes++;
   else
      fails++;
   printf("  [%s] ", cond ? "PASS" : "FAIL");
   va_start(ap, fmt);
   vprintf(fmt, ap);
   va_end(ap);
   printf("\n");
}

static uint32_t
mem_type(uint32_t bits, VkMemoryPropertyFlags want)
{
   for (uint32_t i = 0; i < memprops.memoryTypeCount; i++)
      if ((bits & (1u << i)) &&
          (memprops.memoryTypes[i].propertyFlags & want) == want)
         return i;
   printf("FATAL no memory type for 0x%x / 0x%x\n", bits, want);
   exit(1);
}

static VkDeviceMemory
alloc_mem(VkDeviceSize size, uint32_t bits, VkMemoryPropertyFlags props)
{
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = size,
      .memoryTypeIndex = mem_type(bits, props),
   };
   VkDeviceMemory m;
   CHECK(vkAllocateMemory(dev, &ai, NULL, &m));
   return m;
}

/* A host-visible staging buffer, for reading results back. */
static VkBuffer
staging(VkDeviceSize size, VkDeviceMemory *mem_out, void **map_out)
{
   VkBufferCreateInfo bi = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
   };
   VkBuffer b;
   CHECK(vkCreateBuffer(dev, &bi, NULL, &b));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, b, &mr);
   *mem_out = alloc_mem(mr.size, mr.memoryTypeBits,
                        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                           VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
   CHECK(vkBindBufferMemory(dev, b, *mem_out, 0));
   CHECK(vkMapMemory(dev, *mem_out, 0, VK_WHOLE_SIZE, 0, map_out));
   return b;
}

static VkCommandBuffer
begin_cmd(void)
{
   VkCommandBufferAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &ai, &cmd));
   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cmd, &bi));
   return cmd;
}

static void
end_cmd(VkCommandBuffer cmd)
{
   CHECK(vkEndCommandBuffer(cmd));
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
   CHECK(vkQueueWaitIdle(queue));
   vkFreeCommandBuffers(dev, pool, 1, &cmd);
}

/* vkQueueBindSparse, waited to completion with a fence so the following
 * submission genuinely observes the new mapping. */
static void
bind_sparse(const VkBindSparseInfo *info)
{
   VkFenceCreateInfo fi = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   VkFence fence;
   CHECK(vkCreateFence(dev, &fi, NULL, &fence));
   VkResult r = vkQueueBindSparse(queue, 1, info, fence);
   if (r != VK_SUCCESS) {
      ok(0, "vkQueueBindSparse -> %d", r);
      vkDestroyFence(dev, fence, NULL);
      return;
   }
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
   vkDestroyFence(dev, fence, NULL);
}

/* ---------------------------------------------------------------- buffers -- */
static void
test_buffer(void)
{
   printf("\nsparse BUFFER: 8 blocks, backed from two allocations with a hole\n");

   const uint32_t NB = 8;
   VkBufferCreateInfo bi = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = block_B * NB,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
      .flags = VK_BUFFER_CREATE_SPARSE_BINDING_BIT |
               VK_BUFFER_CREATE_SPARSE_RESIDENCY_BIT,
   };
   VkBuffer buf;
   VkResult r = vkCreateBuffer(dev, &bi, NULL, &buf);
   if (r != VK_SUCCESS) {
      ok(0, "vkCreateBuffer with SPARSE_BINDING|SPARSE_RESIDENCY -> %d", r);
      return;
   }
   ok(1, "vkCreateBuffer with SPARSE_BINDING|SPARSE_RESIDENCY");

   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, buf, &mr);
   ok(mr.alignment == block_B,
      "buffer memory requirement alignment %llu == sparse block %llu",
      (unsigned long long)mr.alignment, (unsigned long long)block_B);

   /* Two independent allocations. Blocks 0..2 come from A, 4..5 from B, and
    * 3, 6, 7 are left unbacked on purpose. */
   VkDeviceMemory memA = alloc_mem(block_B * 4, mr.memoryTypeBits,
                                   VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
   VkDeviceMemory memB = alloc_mem(block_B * 4, mr.memoryTypeBits,
                                   VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);

   VkSparseMemoryBind binds[] = {
      {.resourceOffset = 0, .size = block_B * 3, .memory = memA, .memoryOffset = 0},
      /* Note the non-zero memoryOffset: block 4 lands on B's SECOND block, which
       * is the part a tile-pool implementation could not honour. */
      {.resourceOffset = block_B * 4, .size = block_B * 2, .memory = memB,
       .memoryOffset = block_B},
   };
   VkSparseBufferMemoryBindInfo bbi = {
      .buffer = buf, .bindCount = 2, .pBinds = binds};
   VkBindSparseInfo bsi = {
      .sType = VK_STRUCTURE_TYPE_BIND_SPARSE_INFO,
      .bufferBindCount = 1,
      .pBufferBinds = &bbi,
   };
   bind_sparse(&bsi);
   ok(1, "vkQueueBindSparse: 3 blocks from A, 2 from B at memoryOffset=1 block");

   /* Fill each block with its index, then copy the whole buffer out. */
   VkDeviceMemory smem;
   void *smap;
   VkBuffer stage = staging(block_B * NB, &smem, &smap);

   /* Poison the destination first. Fresh VkDeviceMemory reads as zero, so
    * without this a copy that silently skips an unbacked source range would look
    * exactly like one that correctly read zeros out of it. */
   memset(smap, 0xEE, block_B * NB);

   VkCommandBuffer cmd = begin_cmd();
   for (uint32_t i = 0; i < NB; i++)
      vkCmdFillBuffer(cmd, buf, block_B * i, block_B, 0xA5A50000u + i);
   VkMemoryBarrier mb = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL, 0,
                        NULL);
   VkBufferCopy copy = {.size = block_B * NB};
   vkCmdCopyBuffer(cmd, buf, stage, 1, &copy);
   end_cmd(cmd);

   const uint32_t *got = smap;
   const int backed[8] = {1, 1, 1, 0, 1, 1, 0, 0};
   int data_ok = 1, holes_ok = 1;
   for (uint32_t i = 0; i < NB; i++) {
      uint32_t v = got[i * (block_B / 4)];
      uint32_t want = backed[i] ? 0xA5A50000u + i : 0u;
      if (v != want) {
         if (backed[i])
            data_ok = 0;
         else
            holes_ok = 0;
         printf("       block %u %-8s read 0x%08x want 0x%08x\n", i,
                backed[i] ? "backed" : "UNBACKED", v, want);
      }
   }
   ok(data_ok, "backed blocks read back their fill, across BOTH allocations");
   ok(holes_ok, "unbacked blocks read zero (sparseResidencyNonResidentStrict)");

   /* Rebind block 0 onto memB block 3, which still holds nothing, and confirm
    * the buffer follows the new mapping rather than the old memory. */
   VkSparseMemoryBind rebind = {.resourceOffset = 0,
                                .size = block_B,
                                .memory = memB,
                                .memoryOffset = block_B * 3};
   bbi.bindCount = 1;
   bbi.pBinds = &rebind;
   bind_sparse(&bsi);

   memset(smap, 0xEE, block_B * NB);
   cmd = begin_cmd();
   vkCmdFillBuffer(cmd, buf, 0, block_B, 0x5C5C0001u);
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL, 0,
                        NULL);
   VkBufferCopy c2 = {.size = block_B * 2};
   vkCmdCopyBuffer(cmd, buf, stage, 1, &c2);
   end_cmd(cmd);
   ok(got[0] == 0x5C5C0001u, "rebinding block 0 to other memory takes effect (0x%08x)",
      got[0]);
   ok(got[block_B / 4] == 0xA5A50001u,
      "rebinding block 0 left block 1 alone (0x%08x)", got[block_B / 4]);

   /* Unbind block 1 and watch it go back to reading zero. */
   VkSparseMemoryBind unbind = {
      .resourceOffset = block_B, .size = block_B, .memory = VK_NULL_HANDLE};
   bbi.pBinds = &unbind;
   bind_sparse(&bsi);
   memset(smap, 0xEE, block_B * NB);
   cmd = begin_cmd();
   vkCmdCopyBuffer(cmd, buf, stage, 1, &c2);
   end_cmd(cmd);
   ok(got[block_B / 4] == 0u,
      "unbinding block 1 makes it read zero (0x%08x; 0xeeeeeeee means the copy "
      "skipped the range rather than reading zeros from it)",
      got[block_B / 4]);

   vkDestroyBuffer(dev, buf, NULL);
   vkDestroyBuffer(dev, stage, NULL);
   vkUnmapMemory(dev, smem);
   vkFreeMemory(dev, smem, NULL);
   vkFreeMemory(dev, memA, NULL);
   vkFreeMemory(dev, memB, NULL);
}

/* The shortest possible unmap: one fresh buffer, one block, map, write, unmap,
 * read. Isolates unbinding from every other bind in test_buffer(). */
static void
test_unmap_minimal(void)
{
   printf("\nminimal UNBIND: map one block, write it, unbind it, read it\n");
   VkBufferCreateInfo bi = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = block_B * 2,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
      .flags = VK_BUFFER_CREATE_SPARSE_BINDING_BIT |
               VK_BUFFER_CREATE_SPARSE_RESIDENCY_BIT,
   };
   VkBuffer buf;
   CHECK(vkCreateBuffer(dev, &bi, NULL, &buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, buf, &mr);
   VkDeviceMemory mem = alloc_mem(block_B * 2, mr.memoryTypeBits,
                                  VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);

   VkSparseMemoryBind b = {
      .resourceOffset = 0, .size = block_B, .memory = mem, .memoryOffset = 0};
   VkSparseBufferMemoryBindInfo bbi = {.buffer = buf, .bindCount = 1, .pBinds = &b};
   VkBindSparseInfo bsi = {.sType = VK_STRUCTURE_TYPE_BIND_SPARSE_INFO,
                           .bufferBindCount = 1,
                           .pBufferBinds = &bbi};
   bind_sparse(&bsi);

   VkDeviceMemory smem;
   void *smap;
   VkBuffer stage = staging(block_B, &smem, &smap);
   const uint32_t *got = smap;
   VkBufferCopy copy = {.size = block_B};

   memset(smap, 0xEE, block_B);
   VkCommandBuffer cmd = begin_cmd();
   vkCmdFillBuffer(cmd, buf, 0, block_B, 0x11112222u);
   VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                         .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                         .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT};
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL, 0,
                        NULL);
   vkCmdCopyBuffer(cmd, buf, stage, 1, &copy);
   end_cmd(cmd);
   ok(got[0] == 0x11112222u, "bound block reads its fill (0x%08x)", got[0]);

   b.memory = VK_NULL_HANDLE;
   bind_sparse(&bsi);
   memset(smap, 0xEE, block_B);
   cmd = begin_cmd();
   vkCmdCopyBuffer(cmd, buf, stage, 1, &copy);
   end_cmd(cmd);
   uint32_t first_read = got[0];
   ok(first_read == 0u, "after unbinding, the block reads zero (0x%08x)",
      first_read);

   /* If the unmap merely lags a submission, a second identical copy sees zero.
    * That distinguishes "the unmap never happened" from "it happened late". */
   memset(smap, 0xEE, block_B);
   cmd = begin_cmd();
   vkCmdCopyBuffer(cmd, buf, stage, 1, &copy);
   end_cmd(cmd);
   ok(got[0] == 0u, "one submission later, the block reads zero (0x%08x)", got[0]);

   vkDestroyBuffer(dev, buf, NULL);
   vkDestroyBuffer(dev, stage, NULL);
   vkUnmapMemory(dev, smem);
   vkFreeMemory(dev, smem, NULL);
   vkFreeMemory(dev, mem, NULL);
}

/* ----------------------------------------------------------------- images -- */
static void
test_image(void)
{
   printf("\nsparse IMAGE: 2D RGBA8, one tile bound, the rest a hole\n");

   VkImageCreateInfo ii = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = {512, 512, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
      .flags = VK_IMAGE_CREATE_SPARSE_BINDING_BIT |
               VK_IMAGE_CREATE_SPARSE_RESIDENCY_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };

   /* The format query must agree with vkCreateImage, so ask it first. */
   uint32_t nprops = 0;
   vkGetPhysicalDeviceSparseImageFormatProperties(
      pdev, ii.format, ii.imageType, ii.samples, ii.usage, ii.tiling, &nprops,
      NULL);
   ok(nprops == 1, "vkGetPhysicalDeviceSparseImageFormatProperties -> %u entries",
      nprops);
   VkSparseImageFormatProperties fprops = {0};
   if (nprops >= 1) {
      nprops = 1;
      vkGetPhysicalDeviceSparseImageFormatProperties(pdev, ii.format,
                                                     ii.imageType, ii.samples,
                                                     ii.usage, ii.tiling,
                                                     &nprops, &fprops);
      printf("       granularity %ux%ux%u flags 0x%x\n",
             fprops.imageGranularity.width, fprops.imageGranularity.height,
             fprops.imageGranularity.depth, fprops.flags);
      ok(fprops.imageGranularity.width == 128 &&
            fprops.imageGranularity.height == 128,
         "RGBA8 granularity is the Vulkan standard 128x128 block");
      ok(!(fprops.flags & VK_SPARSE_IMAGE_FORMAT_NONSTANDARD_BLOCK_SIZE_BIT),
         "standard block size reported for a 2D single-sample image");
   }

   VkImage img;
   VkResult r = vkCreateImage(dev, &ii, NULL, &img);
   if (r != VK_SUCCESS) {
      ok(0, "vkCreateImage with SPARSE_BINDING|SPARSE_RESIDENCY -> %d", r);
      return;
   }
   ok(1, "vkCreateImage with SPARSE_BINDING|SPARSE_RESIDENCY");

   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, img, &mr);

   uint32_t nreq = 0;
   vkGetImageSparseMemoryRequirements(dev, img, &nreq, NULL);
   ok(nreq == 1, "vkGetImageSparseMemoryRequirements -> %u entries", nreq);
   VkSparseImageMemoryRequirements sreq = {0};
   if (nreq >= 1) {
      nreq = 1;
      vkGetImageSparseMemoryRequirements(dev, img, &nreq, &sreq);
      printf("       size %llu align %llu | granularity %ux%ux%u tailFirstLod %u "
             "tailSize %llu tailOffset %llu\n",
             (unsigned long long)mr.size, (unsigned long long)mr.alignment,
             sreq.formatProperties.imageGranularity.width,
             sreq.formatProperties.imageGranularity.height,
             sreq.formatProperties.imageGranularity.depth,
             sreq.imageMipTailFirstLod,
             (unsigned long long)sreq.imageMipTailSize,
             (unsigned long long)sreq.imageMipTailOffset);
      /* 512x512 at 128x128 tiles with one mip level: 16 tiles, no tail. */
      ok(mr.size == block_B * 16,
         "512x512 RGBA8 needs 16 sparse blocks (%llu == %llu)",
         (unsigned long long)mr.size, (unsigned long long)(block_B * 16));
      ok(sreq.imageMipTailSize == 0,
         "a single-level image has no mip tail (%llu)",
         (unsigned long long)sreq.imageMipTailSize);
   }

   VkDeviceMemory mem = alloc_mem(block_B * 4, mr.memoryTypeBits,
                                  VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);

   /* Bind exactly the top-left tile, and nothing else. */
   VkSparseImageMemoryBind ibind = {
      .subresource = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
                      .mipLevel = 0,
                      .arrayLayer = 0},
      .offset = {0, 0, 0},
      .extent = {fprops.imageGranularity.width, fprops.imageGranularity.height, 1},
      .memory = mem,
      .memoryOffset = 0,
   };
   VkSparseImageMemoryBindInfo imbi = {
      .image = img, .bindCount = 1, .pBinds = &ibind};
   VkBindSparseInfo bsi = {
      .sType = VK_STRUCTURE_TYPE_BIND_SPARSE_INFO,
      .imageBindCount = 1,
      .pImageBinds = &imbi,
   };
   bind_sparse(&bsi);
   ok(1, "vkQueueBindSparse: one image tile via VkSparseImageMemoryBindInfo");

   /* Clear the whole image, then read back one texel from the bound tile and one
    * from an unbound tile. */
   VkDeviceMemory smem;
   void *smap;
   VkBuffer stage = staging(512 * 512 * 4, &smem, &smap);
   memset(smap, 0xEE, 512 * 512 * 4);

   VkCommandBuffer cmd = begin_cmd();
   VkImageMemoryBarrier to_dst = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
      .image = img,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
      .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &to_dst);
   VkClearColorValue clear = {.float32 = {1.0f, 0.0f, 0.0f, 1.0f}};
   VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
   vkCmdClearColorImage(cmd, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &clear,
                        1, &range);
   VkImageMemoryBarrier to_src = to_dst;
   to_src.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
   to_src.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
   to_src.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   to_src.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &to_src);
   VkBufferImageCopy bic = {
      .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
      .imageExtent = {512, 512, 1},
   };
   vkCmdCopyImageToBuffer(cmd, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, stage,
                          1, &bic);
   end_cmd(cmd);

   const uint32_t *px = smap;
   uint32_t inside = px[0];                 /* (0,0), bound */
   uint32_t outside = px[300 * 512 + 300];  /* (300,300), never bound */
   ok(inside == 0xFF0000FFu, "bound tile holds the clear colour (0x%08x)", inside);
   ok(outside == 0u, "unbound tile reads zero (0x%08x)", outside);

   vkDestroyImage(dev, img, NULL);
   vkDestroyBuffer(dev, stage, NULL);
   vkUnmapMemory(dev, smem);
   vkFreeMemory(dev, smem, NULL);
   vkFreeMemory(dev, mem, NULL);
}

/* --------------------------------------------------------------- mip tail -- */
static void
test_mip_tail(void)
{
   printf("\nsparse IMAGE with mip levels: the tail must be described and bindable\n");

   VkImageCreateInfo ii = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = {512, 512, 1},
      .mipLevels = 10,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
      .flags = VK_IMAGE_CREATE_SPARSE_BINDING_BIT |
               VK_IMAGE_CREATE_SPARSE_RESIDENCY_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage img;
   VkResult r = vkCreateImage(dev, &ii, NULL, &img);
   if (r != VK_SUCCESS) {
      ok(0, "vkCreateImage mipped sparse -> %d", r);
      return;
   }

   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, img, &mr);
   uint32_t nreq = 1;
   VkSparseImageMemoryRequirements sreq = {0};
   vkGetImageSparseMemoryRequirements(dev, img, &nreq, &sreq);
   printf("       size %llu | tailFirstLod %u tailSize %llu tailOffset %llu "
          "tailStride %llu\n",
          (unsigned long long)mr.size, sreq.imageMipTailFirstLod,
          (unsigned long long)sreq.imageMipTailSize,
          (unsigned long long)sreq.imageMipTailOffset,
          (unsigned long long)sreq.imageMipTailStride);
   ok(sreq.imageMipTailFirstLod > 0 && sreq.imageMipTailFirstLod < ii.mipLevels,
      "mip tail starts inside the level range (lod %u of %u)",
      sreq.imageMipTailFirstLod, ii.mipLevels);
   ok(sreq.imageMipTailSize > 0, "mip tail has a size (%llu)",
      (unsigned long long)sreq.imageMipTailSize);
   ok(sreq.imageMipTailOffset % block_B == 0,
      "mip tail offset is block aligned (%llu %% %llu)",
      (unsigned long long)sreq.imageMipTailOffset,
      (unsigned long long)block_B);
   ok(mr.size >= sreq.imageMipTailOffset + sreq.imageMipTailSize,
      "memory requirement covers the tail (%llu >= %llu)",
      (unsigned long long)mr.size,
      (unsigned long long)(sreq.imageMipTailOffset + sreq.imageMipTailSize));

   /* Bind the tail opaquely — the path with no Metal counterpart, so the one
    * most worth exercising. */
   VkDeviceMemory mem = alloc_mem(mr.size, mr.memoryTypeBits,
                                  VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
   VkSparseMemoryBind tail = {
      .resourceOffset = sreq.imageMipTailOffset,
      .size = sreq.imageMipTailSize,
      .memory = mem,
      .memoryOffset = sreq.imageMipTailOffset,
   };
   VkSparseImageOpaqueMemoryBindInfo obi = {
      .image = img, .bindCount = 1, .pBinds = &tail};
   VkBindSparseInfo bsi = {
      .sType = VK_STRUCTURE_TYPE_BIND_SPARSE_INFO,
      .imageOpaqueBindCount = 1,
      .pImageOpaqueBinds = &obi,
   };
   bind_sparse(&bsi);
   ok(1, "vkQueueBindSparse: mip tail via VkSparseImageOpaqueMemoryBindInfo");

   /* And bind every tiled level opaquely too, i.e. the whole flat range. That is
    * what a sparseBinding-only application does. */
   VkSparseMemoryBind all = {
      .resourceOffset = 0, .size = mr.size, .memory = mem, .memoryOffset = 0};
   obi.pBinds = &all;
   bind_sparse(&bsi);
   ok(1, "vkQueueBindSparse: the whole image opaquely, tiled levels and tail");

   /* Now clear the top level and read it back: the opaque whole-image bind must
    * have produced a fully backed image. */
   VkDeviceMemory smem;
   void *smap;
   VkBuffer stage = staging(512 * 512 * 4, &smem, &smap);
   memset(smap, 0xEE, 512 * 512 * 4);

   VkCommandBuffer cmd = begin_cmd();
   VkImageMemoryBarrier b = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
      .image = img,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, ii.mipLevels, 0, 1},
      .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &b);
   VkClearColorValue clear = {.float32 = {0.0f, 1.0f, 0.0f, 1.0f}};
   VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, ii.mipLevels,
                                    0, 1};
   vkCmdClearColorImage(cmd, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &clear,
                        1, &range);
   VkImageMemoryBarrier b2 = b;
   b2.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
   b2.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
   b2.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   b2.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &b2);
   VkBufferImageCopy bic = {
      .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
      .imageExtent = {512, 512, 1},
   };
   vkCmdCopyImageToBuffer(cmd, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, stage,
                          1, &bic);
   end_cmd(cmd);

   const uint32_t *px = smap;
   int all_green = 1;
   for (uint32_t i = 0; i < 512u * 512u; i += 4099)
      if (px[i] != 0xFF00FF00u) {
         all_green = 0;
         break;
      }
   ok(all_green,
      "after an opaque whole-image bind, every sampled texel of level 0 is "
      "backed (first mismatch check, px[0]=0x%08x)",
      px[0]);

   vkDestroyImage(dev, img, NULL);
   vkDestroyBuffer(dev, stage, NULL);
   vkUnmapMemory(dev, smem);
   vkFreeMemory(dev, smem, NULL);
   vkFreeMemory(dev, mem, NULL);
}

int
main(void)
{
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_1,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t n = 0;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, NULL));
   if (n == 0) {
      printf("no physical device\n");
      return 1;
   }
   VkPhysicalDevice devs[8];
   n = n > 8 ? 8 : n;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, devs));
   pdev = devs[0];

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);
   vkGetPhysicalDeviceMemoryProperties(pdev, &memprops);
   printf("device: %s (api %u.%u.%u)\n", props.deviceName,
          VK_VERSION_MAJOR(props.apiVersion), VK_VERSION_MINOR(props.apiVersion),
          VK_VERSION_PATCH(props.apiVersion));

   printf("\nadvertisement\n");
   VkPhysicalDeviceFeatures feat;
   vkGetPhysicalDeviceFeatures(pdev, &feat);
   printf("       sparseBinding %d  residencyBuffer %d  image2D %d  image3D %d  "
          "aliased %d\n",
          feat.sparseBinding, feat.sparseResidencyBuffer,
          feat.sparseResidencyImage2D, feat.sparseResidencyImage3D,
          feat.sparseResidencyAliased);
   printf("       residency 2/4/8/16 samples %d/%d/%d/%d  shaderResourceResidency %d\n",
          feat.sparseResidency2Samples, feat.sparseResidency4Samples,
          feat.sparseResidency8Samples, feat.sparseResidency16Samples,
          feat.shaderResourceResidency);
   printf("       properties: nonResidentStrict %d std2D %d std2DMS %d std3D %d "
          "alignedMipSize %d\n",
          props.sparseProperties.residencyNonResidentStrict,
          props.sparseProperties.residencyStandard2DBlockShape,
          props.sparseProperties.residencyStandard2DMultisampleBlockShape,
          props.sparseProperties.residencyStandard3DBlockShape,
          props.sparseProperties.residencyAlignedMipSize);
   ok(feat.sparseBinding, "sparseBinding advertised");
   ok(feat.sparseResidencyBuffer, "sparseResidencyBuffer advertised");
   ok(feat.sparseResidencyImage2D, "sparseResidencyImage2D advertised");
   /* Deliberately inverted, and a tripwire rather than a nicety. The driver
    * withholds residencyNonResidentStrict because unbinding a range does not
    * restore reads-as-zero (see kk_sparse.c). If unbinding is ever fixed, the
    * three unbind checks below start passing and THIS one starts failing, which
    * forces both this file's header and kk_physical_device.c to be revisited
    * instead of quietly drifting out of date. */
   ok(!props.sparseProperties.residencyNonResidentStrict,
      "residencyNonResidentStrict withheld, matching the unbind limitation");

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, NULL);
   VkQueueFamilyProperties qf[8];
   nq = nq > 8 ? 8 : nq;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   for (uint32_t i = 0; i < nq; i++)
      if (qf[i].queueFlags & VK_QUEUE_SPARSE_BINDING_BIT) {
         qfam = i;
         break;
      }
   ok(qfam != UINT32_MAX, "a queue family advertises VK_QUEUE_SPARSE_BINDING_BIT");
   if (qfam == UINT32_MAX)
      goto done;

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfam,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkPhysicalDeviceFeatures want = {
      .sparseBinding = VK_TRUE,
      .sparseResidencyBuffer = feat.sparseResidencyBuffer,
      .sparseResidencyImage2D = feat.sparseResidencyImage2D,
      .sparseResidencyAliased = feat.sparseResidencyAliased,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .pEnabledFeatures = &want,
   };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   ok(1, "vkCreateDevice with the sparse features enabled");
   vkGetDeviceQueue(dev, qfam, 0, &queue);

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qfam,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
   };
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));

   /* The sparse block size, read out of a sparse buffer's own requirements
    * rather than assumed to be 65536. */
   VkBufferCreateInfo probe = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = 1 << 20,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .flags = VK_BUFFER_CREATE_SPARSE_BINDING_BIT,
   };
   VkBuffer pb;
   CHECK(vkCreateBuffer(dev, &probe, NULL, &pb));
   VkMemoryRequirements pmr;
   vkGetBufferMemoryRequirements(dev, pb, &pmr);
   block_B = pmr.alignment;
   vkDestroyBuffer(dev, pb, NULL);
   printf("       sparse block size %llu B\n", (unsigned long long)block_B);
   ok(block_B == 65536,
      "sparse block size is Vulkan's standard 65536 (%llu)",
      (unsigned long long)block_B);

   test_buffer();
   test_unmap_minimal();
   test_image();
   test_mip_tail();

done:
   printf("\n%d/%d checks passed\n", passes, passes + fails);
   return fails != 0;
}
