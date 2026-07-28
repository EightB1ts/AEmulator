// wrapper.h — bindgen entry point for the gfxstream host-renderer C ABI (Phase 3a).
//
// Parsed as C (bindgen --  -x c), so the `#ifdef __cplusplus` C++-only includes inside
// opengles.h (RenderLib.h, opengl/misc.h) are skipped. The include paths in gen-ffi.sh
// resolve the remaining aemu / host-common / render-utils headers.
//
// Covers the full 3c transport ABI:
//   virtio-gpu-gfxstream-renderer.h          — stream_renderer_init / _context_create /
//                                               _submit_cmd / _create_fence / resources / blobs
//   virtio-gpu-gfxstream-renderer-unstable.h — gfxstream_backend_setup_window + unstable extras
//   host-common/opengles.h                   — android_* opengles setup + display/window ops

#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>

// opengles.h line 128 declares one function with the EMUGL_COMMON_API export macro, which is
// only defined by a C++/build-system header we deliberately don't pull in. Neutralize it.
#ifndef EMUGL_COMMON_API
#define EMUGL_COMMON_API
#endif

#include "gfxstream/virtio-gpu-gfxstream-renderer.h"
#include "gfxstream/virtio-gpu-gfxstream-renderer-unstable.h"
#include "host-common/opengles.h"
