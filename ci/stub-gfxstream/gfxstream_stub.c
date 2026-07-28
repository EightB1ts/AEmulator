/*
 * gfxstream_stub.c — the CI LINK STUB for libgfxstream_backend.
 *
 * WHAT THIS IS NOT: a renderer, a software fallback, a "headless mode", or anything you should ever
 * have on a library search path outside CI. It draws nothing, allocates nothing, and every entry
 * point fails.
 *
 * WHY IT EXISTS: `crates/aemulator-hvf` is workspace-excluded and its build.rs links
 * `-lgfxstream_backend` UNCONDITIONALLY on macOS (there is deliberately no cargo feature to turn the
 * GPU path off — see the invariant comment in crates/aemulator-hvf/Cargo.toml). The real dylib is a
 * ~45-minute gfxstream + Mesa build, which a hosted GitHub runner will not do. But every test in that
 * crate is hermetic: mock renderer, mock guest mapper, in-process TestQueue. None of them *call* into
 * the dylib — they only need the crate to LINK. So a dylib that exports the same symbols as loud
 * no-ops is sufficient to turn ~10k previously-uncovered lines into CI-covered lines.
 *
 * SAFETY PROPERTY: a silently-working stub is the dangerous outcome. This repo has already been
 * burned by a binary that booted fine and was quietly crippled (a `--features gfxstream`-only build
 * where the guest took a DHCP lease and no packet ever crossed the wire — it presented as "adb is
 * broken", not "wrong binary"). So:
 *   - a constructor prints a banner at dyld load time, before main, whether or not anything is called;
 *   - every entry point prints a loud line to stderr naming itself on first call;
 *   - stream_renderer_init() returns NON-ZERO, so the VMM's own error path refuses to proceed;
 *   - no out-parameter is ever written, so nothing downstream can read a plausible-looking value.
 *
 * The symbol set is the authoritative import list of crates/aemulator-hvf/src/gfxstream/ffi.rs
 * (bindgen output). It is NOT hand-guessed and it is NOT allowed to drift: ci/stub-gfxstream/build.sh
 * re-derives the list from ffi.rs and fails, naming the missing symbols, if this file does not export
 * every one. See README.md in this directory.
 *
 * Parameter lists mirror ffi.rs for documentation value only; struct pointers are spelled `void *`
 * because the stub never dereferences anything. Return TYPES do matter (int vs void vs pointer) and
 * are faithful.
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

#define AEMU_STUB_BANNER "*** AEMU CI LINK STUB ***"

static void aemu_stub_announce(const char *fn, int *once) {
    /* Cheap idempotence: at worst two threads both print, which is still correct behaviour. */
    if (*once) {
        return;
    }
    *once = 1;
    fprintf(stderr,
            "\n%s %s() CALLED — THIS IS THE CI LINK STUB, NOT A RENDERER.\n"
            "%s Nothing is being rendered. This library (ci/stub-gfxstream) exists ONLY to satisfy\n"
            "%s the linker in CI so the hermetic tests of crates/aemulator-hvf can build and run.\n"
            "%s If you are seeing this outside CI your library path is WRONG. Build the real thing:\n"
            "%s     third_party/bootstrap.sh gfxstream\n"
            "%s and unset GFXSTREAM_BUILD_HOST.\n\n",
            AEMU_STUB_BANNER, fn, AEMU_STUB_BANNER, AEMU_STUB_BANNER, AEMU_STUB_BANNER,
            AEMU_STUB_BANNER, AEMU_STUB_BANNER);
    fflush(stderr);
}

/*
 * One static flag per function. Declared inside the function body so each entry point gets its own
 * first-call latch and the name in the message is always the function you actually reached.
 */
#define AEMU_STUB(name)                                                                            \
    do {                                                                                           \
        static int aemu_stub_once = 0;                                                             \
        aemu_stub_announce(#name, &aemu_stub_once);                                                 \
    } while (0)

/*
 * Loud at load time, not just at call time. A test binary that links the stub and never touches the
 * renderer still says so on stderr — which is exactly the case we want visible in CI logs, and the
 * case that would otherwise look identical to a real build.
 */
__attribute__((constructor)) static void aemu_stub_loaded(void) {
    fprintf(stderr,
            "\n%s libgfxstream_backend LOADED FROM THE CI LINK STUB — NOT A RENDERER.\n"
            "%s Source: ci/stub-gfxstream/gfxstream_stub.c. Every entry point fails.\n\n",
            AEMU_STUB_BANNER, AEMU_STUB_BANNER);
    fflush(stderr);
}

/* -1: a benign, unambiguous failure. gfxstream returns 0 on success everywhere. */
#define AEMU_STUB_FAIL (-1)

/* ------------------------------------------------------------------ stream_renderer_* (lifecycle) */

void stream_renderer_log(uint32_t type, const char *file, int line, const char *pretty_function,
                         const char *format, ...) {
    AEMU_STUB(stream_renderer_log);
}

/*
 * NON-ZERO on purpose, and this is the load-bearing one. gfxstream's contract is 0 == success, so the
 * VMM's own `if init() != 0 { bail }` refuses to run a graphics stack that does not exist. A stub that
 * returned 0 here would let a build boot and be quietly crippled — the exact failure mode this repo
 * removed cargo features to prevent.
 */
int stream_renderer_init(void *stream_renderer_params, uint64_t num_params) {
    AEMU_STUB(stream_renderer_init);
    return AEMU_STUB_FAIL;
}

void stream_renderer_teardown(void) { AEMU_STUB(stream_renderer_teardown); }

int stream_renderer_suspend(void) {
    AEMU_STUB(stream_renderer_suspend);
    return AEMU_STUB_FAIL;
}

int stream_renderer_resume(void) {
    AEMU_STUB(stream_renderer_resume);
    return AEMU_STUB_FAIL;
}

int stream_renderer_snapshot(const char *dir) {
    AEMU_STUB(stream_renderer_snapshot);
    return AEMU_STUB_FAIL;
}

int stream_renderer_restore(const char *dir) {
    AEMU_STUB(stream_renderer_restore);
    return AEMU_STUB_FAIL;
}

/* ------------------------------------------------------------------- stream_renderer_* (resources) */

int stream_renderer_resource_create(void *args, void *iov, uint32_t num_iovs) {
    AEMU_STUB(stream_renderer_resource_create);
    return AEMU_STUB_FAIL;
}

void stream_renderer_resource_unref(uint32_t res_handle) {
    AEMU_STUB(stream_renderer_resource_unref);
}

int stream_renderer_resource_attach_iov(int res_handle, void *iov, int num_iovs) {
    AEMU_STUB(stream_renderer_resource_attach_iov);
    return AEMU_STUB_FAIL;
}

/* Writes neither *iov nor *num_iovs — see the "no out-parameter is ever written" rule above. */
void stream_renderer_resource_detach_iov(int res_handle, void **iov, int *num_iovs) {
    AEMU_STUB(stream_renderer_resource_detach_iov);
}

int stream_renderer_resource_get_info(int res_handle, void *info) {
    AEMU_STUB(stream_renderer_resource_get_info);
    return AEMU_STUB_FAIL;
}

int stream_renderer_resource_map(uint32_t res_handle, void **hvaOut, uint64_t *sizeOut) {
    AEMU_STUB(stream_renderer_resource_map);
    return AEMU_STUB_FAIL;
}

int stream_renderer_resource_unmap(uint32_t res_handle) {
    AEMU_STUB(stream_renderer_resource_unmap);
    return AEMU_STUB_FAIL;
}

int stream_renderer_resource_map_info(uint32_t res_handle, uint32_t *map_info) {
    AEMU_STUB(stream_renderer_resource_map_info);
    return AEMU_STUB_FAIL;
}

int stream_renderer_transfer_read_iov(uint32_t handle, uint32_t ctx_id, uint32_t level,
                                      uint32_t stride, uint32_t layer_stride, void *box_,
                                      uint64_t offset, void *iov, int iovec_cnt) {
    AEMU_STUB(stream_renderer_transfer_read_iov);
    return AEMU_STUB_FAIL;
}

int stream_renderer_transfer_write_iov(uint32_t handle, uint32_t ctx_id, int level, uint32_t stride,
                                       uint32_t layer_stride, void *box_, uint64_t offset,
                                       void *iovec, unsigned int iovec_cnt) {
    AEMU_STUB(stream_renderer_transfer_write_iov);
    return AEMU_STUB_FAIL;
}

int stream_renderer_create_blob(uint32_t ctx_id, uint32_t res_handle, const void *create_blob,
                                const void *iovecs, uint32_t num_iovs, const void *handle) {
    AEMU_STUB(stream_renderer_create_blob);
    return AEMU_STUB_FAIL;
}

int stream_renderer_export_blob(uint32_t res_handle, void *handle) {
    AEMU_STUB(stream_renderer_export_blob);
    return AEMU_STUB_FAIL;
}

int stream_renderer_vulkan_info(uint32_t res_handle, void *vulkan_info) {
    AEMU_STUB(stream_renderer_vulkan_info);
    return AEMU_STUB_FAIL;
}

int stream_renderer_wait_sync_resource(uint32_t res_handle) {
    AEMU_STUB(stream_renderer_wait_sync_resource);
    return AEMU_STUB_FAIL;
}

/* -------------------------------------------------- stream_renderer_* (contexts, caps, commands) */

int stream_renderer_context_create(uint32_t ctx_id, uint32_t nlen, const char *name,
                                   uint32_t context_init) {
    AEMU_STUB(stream_renderer_context_create);
    return AEMU_STUB_FAIL;
}

void stream_renderer_context_destroy(uint32_t handle) {
    AEMU_STUB(stream_renderer_context_destroy);
}

void stream_renderer_ctx_attach_resource(int ctx_id, int res_handle) {
    AEMU_STUB(stream_renderer_ctx_attach_resource);
}

void stream_renderer_ctx_detach_resource(int ctx_id, int res_handle) {
    AEMU_STUB(stream_renderer_ctx_detach_resource);
}

int stream_renderer_submit_cmd(void *cmd) {
    AEMU_STUB(stream_renderer_submit_cmd);
    return AEMU_STUB_FAIL;
}

int stream_renderer_create_fence(const void *fence) {
    AEMU_STUB(stream_renderer_create_fence);
    return AEMU_STUB_FAIL;
}

/* Leaves *max_ver and *max_size untouched — the caller's zero-initialised locals read as "no caps". */
void stream_renderer_get_cap_set(uint32_t set, uint32_t *max_ver, uint32_t *max_size) {
    AEMU_STUB(stream_renderer_get_cap_set);
}

void stream_renderer_fill_caps(uint32_t set, uint32_t version, void *caps) {
    AEMU_STUB(stream_renderer_fill_caps);
}

/* ------------------------------------------------------------- present path (Metal / window / EGL) */

void gfxstream_backend_setup_window(void *native_window_handle, int32_t window_x, int32_t window_y,
                                   int32_t window_width, int32_t window_height, int32_t fb_width,
                                   int32_t fb_height) {
    AEMU_STUB(gfxstream_backend_setup_window);
}

int stream_renderer_present_to_metal_layer(uint32_t res_handle, void *ca_metal_layer) {
    AEMU_STUB(stream_renderer_present_to_metal_layer);
    return AEMU_STUB_FAIL;
}

void stream_renderer_release_metal_layer(void *ca_metal_layer) {
    AEMU_STUB(stream_renderer_release_metal_layer);
}

/*
 * The host-side GPU-identity setter (graphics plan Phase 4.6). Called BEFORE stream_renderer_init, and
 * the real implementation's default — setter never called — is exact host passthrough. Failing here is
 * therefore the correct stub behaviour twice over: nothing is spoofed, and init() fails anyway.
 * `identity` carries a struct_size ABI guard; the stub does not read it.
 */
int stream_renderer_set_gpu_identity(const void *identity) {
    AEMU_STUB(stream_renderer_set_gpu_identity);
    return AEMU_STUB_FAIL;
}

void stream_renderer_flush(uint32_t res_handle) { AEMU_STUB(stream_renderer_flush); }

int stream_renderer_platform_import_resource(int res_handle, int res_info, void *resource) {
    AEMU_STUB(stream_renderer_platform_import_resource);
    return AEMU_STUB_FAIL;
}

void *stream_renderer_platform_create_shared_egl_context(void) {
    AEMU_STUB(stream_renderer_platform_create_shared_egl_context);
    return NULL;
}

int stream_renderer_platform_destroy_shared_egl_context(void *context) {
    AEMU_STUB(stream_renderer_platform_destroy_shared_egl_context);
    return AEMU_STUB_FAIL;
}

/* --------------------------------------------------- android_*Opengles* (the legacy emugl surface) */

int android_prepareOpenglesEmulation(void) {
    AEMU_STUB(android_prepareOpenglesEmulation);
    return AEMU_STUB_FAIL;
}

int android_setOpenglesEmulation(void *renderLib, void *eglDispatch, void *glesv2Dispatch) {
    AEMU_STUB(android_setOpenglesEmulation);
    return AEMU_STUB_FAIL;
}

int android_initOpenglesEmulation(void) {
    AEMU_STUB(android_initOpenglesEmulation);
    return AEMU_STUB_FAIL;
}

int android_startOpenglesRenderer(int width, int height, bool isPhone, int guestApiLevel,
                                  const void *vm_operations, const void *window_agent,
                                  const void *multi_display_agent, const void *gfxstreamFeatures,
                                  int *glesMajorVersion_out, int *glesMinorVersion_out) {
    AEMU_STUB(android_startOpenglesRenderer);
    return AEMU_STUB_FAIL;
}

void android_setPostCallback(void *onPost, void *onPostContext, bool useBgraReadback,
                            uint32_t displayId) {
    AEMU_STUB(android_setPostCallback);
}

/* Leaves all three out-pointers untouched: no fabricated GL_VENDOR/GL_RENDERER/GL_VERSION strings. */
void android_getOpenglesHardwareStrings(char **vendor, char **renderer, char **version) {
    AEMU_STUB(android_getOpenglesHardwareStrings);
}

int android_showOpenglesWindow(void *window, int wx, int wy, int ww, int wh, int fbw, int fbh,
                               float dpr, float rotation, bool deleteExisting, bool hideWindow) {
    AEMU_STUB(android_showOpenglesWindow);
    return AEMU_STUB_FAIL;
}

int android_hideOpenglesWindow(void) {
    AEMU_STUB(android_hideOpenglesWindow);
    return AEMU_STUB_FAIL;
}

void android_setOpenglesTranslation(float px, float py) {
    AEMU_STUB(android_setOpenglesTranslation);
}

void android_setOpenglesScreenMask(int width, int height, const unsigned char *rgbaData) {
    AEMU_STUB(android_setOpenglesScreenMask);
}

void android_redrawOpenglesWindow(void) { AEMU_STUB(android_redrawOpenglesWindow); }

void android_stopOpenglesRenderer(bool wait) { AEMU_STUB(android_stopOpenglesRenderer); }

void android_finishOpenglesRenderer(void) { AEMU_STUB(android_finishOpenglesRenderer); }

void android_waitForOpenglesProcessCleanup(void) {
    AEMU_STUB(android_waitForOpenglesProcessCleanup);
}

void android_setOpenglesDisplayConfigs(int configId, int w, int h, int dpiX, int dpiY) {
    AEMU_STUB(android_setOpenglesDisplayConfigs);
}

void android_setOpenglesDisplayActiveConfig(int configId) {
    AEMU_STUB(android_setOpenglesDisplayActiveConfig);
}
