// metal_shim.m — CAMetalLayer attachment for the in-Vulkan present path (Phase KosmicKrisp).
//
// gfxstream now presents into the CAMetalLayer itself — vkCreateMetalSurfaceEXT(pLayer) +
// VK_KHR_swapchain + vkCmdBlitImage + vkQueuePresentKHR, all inside the host renderer's Vulkan
// device. So this shim no longer blits anything on the present hot-path. What it still owns is:
//
//   * **Layer attachment** — take an NSView (from winit, via raw-window-handle) and guarantee it is
//     backed by a correctly configured CAMetalLayer, then hand that layer out as an opaque pointer
//     for the renderer to create its surface on ([`aemu_metal_layer_for_view`]).
//   * **The standalone test pattern** — [`aemu_metal_present_test_pattern`] still drives a full
//     host-texture → drawable → present cycle, which is what `aemu-display-selftest` uses to prove
//     the window/layer plumbing with no guest and no Vulkan involved.
//
// Compiled by build.rs (cc, `-fobjc-arc`) ONLY under `--features gfxstream`. The default build
// never touches Metal/QuartzCore.
//
// All entry points must be called on the process main thread (AppKit + CAMetalLayer requirement);
// the display self-test drives them from inside the winit event loop, which winit pins to main.

#import <AppKit/AppKit.h>
#import <QuartzCore/CAMetalLayer.h>
#import <QuartzCore/CATransaction.h>
#import <Metal/Metal.h>
#import <stdint.h>
#import <stdio.h>

// Cache a single device + queue across presents so repeated frames don't churn Metal objects.
static id<MTLDevice> g_device = nil;
static id<MTLCommandQueue> g_queue = nil;

// Attach (or reuse) a CAMetalLayer on `view` and configure it for a `w`x`h` (POINT) drawable.
// Returns the layer, or nil on failure. Honors the window's backing scale so a Retina drawable is
// pixel-exact. This is the self-test / `aemu_metal_attach_layer` path; the in-Vulkan present
// hand-off uses aemu_metal_layer_for_view, which takes physical pixels and does not bind a queue.
static CAMetalLayer* aemu_ensure_metal_layer(NSView* view, id<MTLDevice> device, int w, int h) {
    if (!view) return nil;
    view.wantsLayer = YES;

    CAMetalLayer* layer = nil;
    if ([view.layer isKindOfClass:[CAMetalLayer class]]) {
        layer = (CAMetalLayer*)view.layer;
    } else {
        layer = [CAMetalLayer layer];
        view.layer = layer;
    }

    CGFloat scale = view.window ? view.window.backingScaleFactor : 1.0;
    layer.device = device;
    layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    // framebufferOnly=NO is required so the drawable's texture can be a blit destination — the test
    // pattern copies a host texture straight into it.
    layer.framebufferOnly = NO;
    layer.contentsScale = scale;
    layer.drawableSize = CGSizeMake((CGFloat)w * scale, (CGFloat)h * scale);
    return layer;
}

// Public: does `view` currently have a CAMetalLayer as its backing layer? (verification hook)
int aemu_metal_layer_attached(void* ns_view) {
    @autoreleasepool {
        NSView* view = (__bridge NSView*)ns_view;
        if (!view) return 0;
        return [view.layer isKindOfClass:[CAMetalLayer class]] ? 1 : 0;
    }
}

// Public: derive the NSWindow* that owns this NSView (gfxstream_backend_setup_window on macOS
// expects an NSWindow*, and rwh 0.6 only hands us the NSView). Returns NULL if not yet installed.
void* aemu_nsview_window(void* ns_view) {
    @autoreleasepool {
        NSView* view = (__bridge NSView*)ns_view;
        if (!view) return NULL;
        return (__bridge void*)view.window;
    }
}

// Public: attach a CAMetalLayer to the view (idempotent). Returns 1 on success. Used by the
// gfxstream path to guarantee the layer exists before handing the window to gfxstream.
int aemu_metal_attach_layer(void* ns_view, int w, int h) {
    @autoreleasepool {
        NSView* view = (__bridge NSView*)ns_view;
        if (!view) return 0;
        if (!g_device) g_device = MTLCreateSystemDefaultDevice();
        if (!g_device) return 0;
        CAMetalLayer* layer = aemu_ensure_metal_layer(view, g_device, w, h);
        return layer ? 1 : 0;
    }
}

// Public: present a host-generated test pattern (per-pixel gradient + 32px checker) into the view's
// CAMetalLayer drawable. This exercises the full presenter our gfxstream frames will use:
// CPU texture -> MTLTexture (replaceRegion) -> blit into CAMetalDrawable -> presentDrawable.
//
// Returns 0 on success; negative on failure (-1 no view, -2 no Metal device, -3 no layer,
// -4 texture alloc, -5 no drawable).
int aemu_metal_present_test_pattern(void* ns_view, int w, int h) {
    @autoreleasepool {
        NSView* view = (__bridge NSView*)ns_view;
        if (!view) return -1;

        if (!g_device) g_device = MTLCreateSystemDefaultDevice();
        if (!g_device) return -2;
        if (!g_queue) g_queue = [g_device newCommandQueue];

        CAMetalLayer* layer = aemu_ensure_metal_layer(view, g_device, w, h);
        if (!layer) return -3;

        int dw = (int)layer.drawableSize.width;
        int dh = (int)layer.drawableSize.height;
        if (dw <= 0 || dh <= 0) return -3;

        MTLTextureDescriptor* desc =
            [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                                                               width:dw
                                                              height:dh
                                                           mipmapped:NO];
        desc.usage = MTLTextureUsageShaderRead;
        id<MTLTexture> tex = [g_device newTextureWithDescriptor:desc];
        if (!tex) return -4;

        // Fill BGRA8: R = horizontal gradient, G = vertical gradient, B = 32px checker.
        size_t stride = (size_t)dw * 4;
        uint8_t* px = (uint8_t*)malloc(stride * (size_t)dh);
        if (!px) return -4;
        for (int y = 0; y < dh; y++) {
            uint8_t* row = px + (size_t)y * stride;
            for (int x = 0; x < dw; x++) {
                uint8_t* p = row + (size_t)x * 4;
                int checker = (((x >> 5) + (y >> 5)) & 1);
                p[0] = (uint8_t)(checker ? 210 : 40);      // B
                p[1] = (uint8_t)((int)y * 255 / dh);       // G
                p[2] = (uint8_t)((int)x * 255 / dw);       // R
                p[3] = 255;                                 // A
            }
        }
        [tex replaceRegion:MTLRegionMake2D(0, 0, dw, dh)
               mipmapLevel:0
                 withBytes:px
               bytesPerRow:stride];
        free(px);

        id<CAMetalDrawable> drawable = [layer nextDrawable];
        if (!drawable) return -5;

        id<MTLCommandBuffer> cb = [g_queue commandBuffer];
        id<MTLBlitCommandEncoder> blit = [cb blitCommandEncoder];
        [blit copyFromTexture:tex
                  sourceSlice:0
                  sourceLevel:0
                 sourceOrigin:MTLOriginMake(0, 0, 0)
                   sourceSize:MTLSizeMake(dw, dh, 1)
                    toTexture:drawable.texture
             destinationSlice:0
             destinationLevel:0
            destinationOrigin:MTLOriginMake(0, 0, 0)];
        [blit endEncoding];
        [cb presentDrawable:drawable];
        [cb commit];
        [cb waitUntilCompleted];
        return 0;
    }
}

// ============================================================================================
// In-Vulkan present hand-off (Phase KosmicKrisp): give the host renderer the CAMetalLayer.
//
// gfxstream now presents into the layer itself — vkCreateMetalSurfaceEXT(pLayer) + VK_KHR_swapchain
// + vkCmdBlitImage + vkQueuePresentKHR — instead of handing us an id<MTLTexture> to blit. So this
// process no longer acquires drawables, no longer needs a blit/bridge pipeline, and no longer needs
// its own MTLCommandQueue for presenting. All it must do is guarantee the NSView is backed by a
// correctly configured CAMetalLayer and return that layer as an opaque pointer.
//
// Idempotent: reuses the existing CAMetalLayer if there is one, and re-applies contentsScale +
// drawableSize, so the same call handles both first attach and window resize. `px_w`/`px_h` are
// PHYSICAL pixels (winit's `Window::inner_size()`), not points.
//
// After the pointer is published, the Vulkan driver owns drawable acquisition. This function is the
// ONLY thing the main thread may do to the layer from then on, and it must run on the main thread
// (AppKit requirement). A drawableSize change makes the driver's next
// vkGetPhysicalDeviceSurfaceCapabilitiesKHR report a new currentExtent, so the renderer's
// acquire/present returns VK_ERROR_OUT_OF_DATE_KHR / VK_SUBOPTIMAL_KHR and it rebuilds its
// swapchain — that is the whole resize protocol.
//
// Returns the CAMetalLayer* (unretained; the NSView owns it), or NULL on failure.
void* aemu_metal_layer_for_view(void* ns_view, int px_w, int px_h) {
    @autoreleasepool {
        NSView* view = (__bridge NSView*)ns_view;
        if (!view || px_w <= 0 || px_h <= 0) return NULL;
        if (!g_device) g_device = MTLCreateSystemDefaultDevice();
        if (!g_device) return NULL;

        view.wantsLayer = YES;

        // ADD the CAMetalLayer as a SUBLAYER; never replace `view.layer`. softbuffer builds its own
        // content sublayer (plus a KVO observer) on whatever `view.layer` was when its surface was
        // created, which happens before we get here — replacing the layer orphans it, and the CPU
        // readback present then draws into a layer that is no longer in the tree. Both present paths
        // have to coexist: the guest renders its boot console through 2D scanout (softbuffer) long
        // before the first 3D flush, and a 2D-only or gpu3d-init-failed run never presents through
        // Vulkan at all.
        //
        // Starts hidden. The display thread reveals it on the first successful in-Vulkan present
        // (aemu_metal_layer_set_visible), so until Vulkan actually puts a frame up, softbuffer owns
        // the window and nothing regresses.
        CAMetalLayer* layer = nil;
        for (CALayer* sub in view.layer.sublayers) {
            if ([sub isKindOfClass:[CAMetalLayer class]]) {
                layer = (CAMetalLayer*)sub;
                break;
            }
        }
        if (!layer) {
            layer = [CAMetalLayer layer];
            layer.hidden = YES;
            layer.frame = view.layer.bounds;
            layer.autoresizingMask = kCALayerWidthSizable | kCALayerHeightSizable;
            [view.layer addSublayer:layer];
        } else {
            layer.frame = view.layer.bounds;
        }

        // Set a device up front so the layer is well formed before the surface is created; the
        // Vulkan driver rebinds it to its own MTLDevice at swapchain creation. On Apple Silicon
        // MTLCreateSystemDefaultDevice() is the same single GPU the driver picks.
        if (!layer.device) layer.device = g_device;
        layer.pixelFormat = MTLPixelFormatBGRA8Unorm;   // what SwapChainStateVk negotiates
        // framebufferOnly = NO: the swapchain images are vkCmdBlitImage destinations
        // (VK_IMAGE_USAGE_TRANSFER_DST_BIT), which Metal will not allow on a framebuffer-only
        // drawable.
        layer.framebufferOnly = NO;
        layer.contentsScale = view.window ? view.window.backingScaleFactor : 1.0;
        layer.drawableSize = CGSizeMake((CGFloat)px_w, (CGFloat)px_h);
        return (__bridge void*)layer;
    }
}

// Reveal (or re-hide) the in-Vulkan present layer. Called by the display thread the first time
// `flush_3d` reports a successful present, which is the only moment we know Vulkan has actually put
// a frame on the drawable; before that the layer must stay hidden so softbuffer's CPU blit shows
// through. Wrapped in an explicit CATransaction with actions disabled: this is called off the main
// thread, and the implicit animation a bare property set would schedule is both pointless here and a
// source of main-thread work we do not want on the vCPU path.
void aemu_metal_layer_set_visible(void* metal_layer, int visible) {
    @autoreleasepool {
        CAMetalLayer* layer = (__bridge CAMetalLayer*)metal_layer;
        if (!layer) return;
        [CATransaction begin];
        [CATransaction setDisableActions:YES];
        layer.hidden = visible ? NO : YES;
        [CATransaction commit];
    }
}
