// probe.c — Phase 3a empirical proof.
//
// Links against libgfxstream_backend.dylib and calls stream_renderer_init() with
// STREAM_RENDERER_FLAGS_USE_VK_BIT, driving the host Vulkan renderer onto Mesa
// KosmicKrisp (Vulkan-on-Metal) on Apple Silicon. Success == stream_renderer_init
// returns 0 AND the log names the Apple M2 physical device, e.g.
//   "Selecting Vulkan device: Apple M2, Version: 1.4.354"
// The version is the tell: KosmicKrisp is a conformant Vulkan 1.3+ driver, so a
// portability-subset-shaped device here would mean the wrong ICD got loaded.
//
// This is an ISOLATION probe: no VMM, no guest, no virtio transport. It exercises
// only the host-side renderer bring-up (the biggest technical risk on the Mac path).
//
// Build + run: see run.sh in this directory.

#include <stdint.h>
#include <stdio.h>
#include <string.h>

// ---- Minimal mirror of the C ABI from
// host/include/gfxstream/virtio-gpu-gfxstream-renderer.h (kept local so the probe
// has no include-path dependency; the real bindings are bindgen'd into ffi.rs). ----

struct stream_renderer_param {
    uint64_t key;
    uint64_t value;
};

struct stream_renderer_fence {
    uint32_t flags;
    uint64_t fence_id;
    uint32_t ctx_id;
    uint8_t ring_idx;
};

typedef void (*stream_renderer_fence_callback)(void* user_data,
                                               struct stream_renderer_fence* fence_data);

// param keys
#define STREAM_RENDERER_PARAM_USER_DATA 1
#define STREAM_RENDERER_PARAM_RENDERER_FLAGS 2
#define STREAM_RENDERER_PARAM_FENCE_CALLBACK 3
#define STREAM_RENDERER_PARAM_WIN0_WIDTH 4
#define STREAM_RENDERER_PARAM_WIN0_HEIGHT 5

// renderer flags
#define STREAM_RENDERER_FLAGS_USE_SURFACELESS_BIT (1u << 3)
#define STREAM_RENDERER_FLAGS_USE_VK_BIT (1u << 5)

extern int stream_renderer_init(struct stream_renderer_param* params, uint64_t num_params);
extern void stream_renderer_teardown(void);

static uint64_t g_user_data = 0xA3A3A3A3A3A3A3A3ull;

static void on_fence(void* user_data, struct stream_renderer_fence* fence) {
    (void)user_data;
    (void)fence;
    // no-op: no fences are produced during a bare init.
}

int main(void) {
    fprintf(stderr, "[probe] starting stream_renderer_init on KosmicKrisp...\n");

    uint64_t flags = STREAM_RENDERER_FLAGS_USE_VK_BIT | STREAM_RENDERER_FLAGS_USE_SURFACELESS_BIT;

    struct stream_renderer_param params[] = {
        {STREAM_RENDERER_PARAM_USER_DATA, (uint64_t)(uintptr_t)&g_user_data},
        {STREAM_RENDERER_PARAM_RENDERER_FLAGS, flags},
        {STREAM_RENDERER_PARAM_FENCE_CALLBACK, (uint64_t)(uintptr_t)&on_fence},
        {STREAM_RENDERER_PARAM_WIN0_WIDTH, 640},
        {STREAM_RENDERER_PARAM_WIN0_HEIGHT, 480},
    };

    int rc = stream_renderer_init(params, sizeof(params) / sizeof(params[0]));
    fprintf(stderr, "[probe] stream_renderer_init returned %d\n", rc);

    if (rc == 0) {
        fprintf(stderr, "[probe] SUCCESS — host renderer initialized.\n");
        stream_renderer_teardown();
        fprintf(stderr, "[probe] teardown complete.\n");
    } else {
        fprintf(stderr, "[probe] FAILURE — init returned nonzero.\n");
    }
    return rc == 0 ? 0 : 1;
}
