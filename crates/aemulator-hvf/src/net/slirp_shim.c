// slirp_shim.c — a thin, ABI-safe bridge between the Rust virtio-net backend and libslirp.
//
// Rationale: SlirpConfig / SlirpCb are large C structs whose layout (embedded `struct in_addr` /
// `struct in6_addr`, many version-gated tail fields) is painful and fragile to replicate in Rust.
// By including <slirp/libslirp.h> here the compiler lays them out for us, so Rust only ever sees a
// tiny hand-rolled, stable C API (the `aemu_slirp_*` functions below).
//
// Threading contract (enforced on the Rust side): libslirp is NOT internally locked. Every call
// into this shim that touches slirp state — aemu_slirp_input / aemu_slirp_add_hostfwd /
// aemu_slirp_fill / aemu_slirp_poll_result / aemu_slirp_cleanup — MUST be made with the Rust-side
// mutex held. The one exception is aemu_slirp_kick() (writes the wake pipe only) and the raw poll()
// syscall Rust runs between fill and poll_result (which touches no slirp state), which run UNLOCKED
// so a guest TX frame can wake the poll thread promptly.
//
// The default network is the standard QEMU user-mode NAT: guest 10.0.2.15/24, gateway/NAT 10.0.2.2,
// DNS 10.0.2.3, built-in DHCP — identical to `qemu -netdev user` and the Android SDK emulator.

#include <slirp/libslirp.h>

#include <poll.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <time.h>
#include <arpa/inet.h>

// Implemented in Rust (src/net/slirp.rs). Delivers a host->guest ethernet frame that libslirp
// produced (a DHCP/ARP/DNS reply or inbound NAT traffic) into the virtio-net RX path. Called with
// the Rust slirp mutex already held (from within slirp_input or slirp_pollfds_poll), so it must not
// re-enter the shim; it only enqueues onto a separate RX buffer.
extern void aemu_slirp_recv_frame(void *opaque, const uint8_t *buf, size_t len);

// A libslirp timer. libslirp (with IPv6 disabled) creates essentially none of these, but we honour
// the v1 timer_new/timer_mod/timer_free contract so any future timer just works. When the deadline
// passes we invoke the stored callback directly (v1 timers, no slirp_handle_timer needed).
typedef struct AemuTimer {
    SlirpTimerCb cb;
    void *cb_opaque;
    int64_t expire_ms;   // 0 == disarmed
    int active;
    struct AemuTimer *next;
} AemuTimer;

#define AEMU_MAX_FDS 512

typedef struct AemuSlirp {
    Slirp *slirp;
    void *rust_opaque;      // passed back to aemu_slirp_recv_frame

    int wake_rd;            // self-pipe read end (always polled at fds[0])
    int wake_wr;            // self-pipe write end (poked by kick()/notify())

    // Poll build state. fds[0] is reserved for the wake pipe; libslirp fds are appended from [1].
    struct pollfd fds[AEMU_MAX_FDS];
    int nfds;

    AemuTimer *timers;
} AemuSlirp;

static int64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// ---- libslirp callbacks (SlirpCb) ----

static slirp_ssize_t cb_send_packet(const void *buf, size_t len, void *opaque) {
    AemuSlirp *a = (AemuSlirp *)opaque;
    aemu_slirp_recv_frame(a->rust_opaque, (const uint8_t *)buf, len);
    return (slirp_ssize_t)len;
}

static void cb_guest_error(const char *msg, void *opaque) {
    (void)opaque;
    fprintf(stderr, "[slirp] guest error: %s\n", msg ? msg : "(null)");
}

static int64_t cb_clock_get_ns(void *opaque) {
    (void)opaque;
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static void *cb_timer_new(SlirpTimerCb cb, void *cb_opaque, void *opaque) {
    AemuSlirp *a = (AemuSlirp *)opaque;
    AemuTimer *t = (AemuTimer *)calloc(1, sizeof(AemuTimer));
    if (!t) return NULL;
    t->cb = cb;
    t->cb_opaque = cb_opaque;
    t->active = 0;
    t->next = a->timers;
    a->timers = t;
    return t;
}

static void cb_timer_free(void *timer, void *opaque) {
    AemuSlirp *a = (AemuSlirp *)opaque;
    AemuTimer **pp = &a->timers;
    while (*pp) {
        if (*pp == (AemuTimer *)timer) {
            AemuTimer *dead = *pp;
            *pp = dead->next;
            free(dead);
            return;
        }
        pp = &(*pp)->next;
    }
}

static void cb_timer_mod(void *timer, int64_t expire_time, void *opaque) {
    (void)opaque;
    AemuTimer *t = (AemuTimer *)timer;
    if (!t) return;
    t->expire_ms = expire_time;
    t->active = 1;
}

static void cb_register_poll_fd(int fd, void *opaque) { (void)fd; (void)opaque; }
static void cb_unregister_poll_fd(int fd, void *opaque) { (void)fd; (void)opaque; }

static void cb_notify(void *opaque) {
    AemuSlirp *a = (AemuSlirp *)opaque;
    if (a->wake_wr >= 0) {
        char b = 'n';
        ssize_t r = write(a->wake_wr, &b, 1);
        (void)r;
    }
}

static const SlirpCb AEMU_CALLBACKS = {
    .send_packet = cb_send_packet,
    .guest_error = cb_guest_error,
    .clock_get_ns = cb_clock_get_ns,
    .timer_new = cb_timer_new,
    .timer_free = cb_timer_free,
    .timer_mod = cb_timer_mod,
    .register_poll_fd = cb_register_poll_fd,
    .unregister_poll_fd = cb_unregister_poll_fd,
    .notify = cb_notify,
};

// ---- fill / poll callbacks ----

static short slirp_to_poll(int events) {
    short e = 0;
    if (events & SLIRP_POLL_IN) e |= POLLIN;
    if (events & SLIRP_POLL_OUT) e |= POLLOUT;
    if (events & SLIRP_POLL_PRI) e |= POLLPRI;
    return e;
}

static int poll_to_slirp(short revents) {
    int e = 0;
    if (revents & POLLIN) e |= SLIRP_POLL_IN;
    if (revents & POLLOUT) e |= SLIRP_POLL_OUT;
    if (revents & POLLPRI) e |= SLIRP_POLL_PRI;
    if (revents & POLLERR) e |= SLIRP_POLL_ERR;
    if (revents & POLLHUP) e |= SLIRP_POLL_HUP;
    if (revents & POLLNVAL) e |= SLIRP_POLL_ERR;
    return e;
}

static int add_poll_cb(slirp_os_socket fd, int events, void *opaque) {
    AemuSlirp *a = (AemuSlirp *)opaque;
    if (a->nfds >= AEMU_MAX_FDS) return -1;
    int idx = a->nfds++;
    a->fds[idx].fd = fd;
    a->fds[idx].events = slirp_to_poll(events);
    a->fds[idx].revents = 0;
    return idx;
}

static int get_revents_cb(int idx, void *opaque) {
    AemuSlirp *a = (AemuSlirp *)opaque;
    if (idx < 0 || idx >= a->nfds) return 0;
    return poll_to_slirp(a->fds[idx].revents);
}

// ---- public shim API (called from Rust) ----

// Create the slirp instance + self-pipe. `rust_opaque` is handed back to aemu_slirp_recv_frame.
// Returns NULL on failure.
AemuSlirp *aemu_slirp_new(void *rust_opaque) {
    AemuSlirp *a = (AemuSlirp *)calloc(1, sizeof(AemuSlirp));
    if (!a) return NULL;
    a->rust_opaque = rust_opaque;

    int p[2];
    if (pipe(p) != 0) { free(a); return NULL; }
    a->wake_rd = p[0];
    a->wake_wr = p[1];
    fcntl(a->wake_rd, F_SETFL, fcntl(a->wake_rd, F_GETFL, 0) | O_NONBLOCK);
    fcntl(a->wake_wr, F_SETFL, fcntl(a->wake_wr, F_GETFL, 0) | O_NONBLOCK);

    SlirpConfig cfg;
    memset(&cfg, 0, sizeof(cfg));
    cfg.version = 1;                 // v1 callbacks (timer_new/notify) — all we need
    cfg.restricted = 0;             // allow internet access
    cfg.in_enabled = true;
    cfg.vnetwork.s_addr = inet_addr("10.0.2.0");
    cfg.vnetmask.s_addr = inet_addr("255.255.255.0");
    cfg.vhost.s_addr = inet_addr("10.0.2.2");        // host / gateway / NAT
    cfg.in6_enabled = false;        // IPv4 only (keeps the RA timer path dormant)
    cfg.vhostname = "aemulator";
    cfg.vdhcp_start.s_addr = inet_addr("10.0.2.15"); // first DHCP lease
    cfg.vnameserver.s_addr = inet_addr("10.0.2.3");  // built-in DNS proxy
    cfg.if_mtu = 0;                 // libslirp default (IF_MTU_DEFAULT)
    cfg.if_mru = 0;
    cfg.disable_host_loopback = false;
    cfg.enable_emu = false;

    a->slirp = slirp_new(&cfg, &AEMU_CALLBACKS, a);
    if (!a->slirp) {
        close(a->wake_rd);
        close(a->wake_wr);
        free(a);
        return NULL;
    }
    return a;
}

// Guest -> host: hand a raw ethernet frame (no virtio_net_hdr) to slirp. May synchronously call
// aemu_slirp_recv_frame (e.g. an ARP/DHCP reply). Wakes the poll thread so any freshly-created
// socket is picked up on the next fill and any synchronous reply is flushed promptly.
void aemu_slirp_input(AemuSlirp *a, const uint8_t *buf, size_t len) {
    slirp_input(a->slirp, buf, (int)len);
    // Wake the poll loop (re-fill with any new fd + flush RX produced above).
    if (a->wake_wr >= 0) { char b = 'i'; ssize_t r = write(a->wake_wr, &b, 1); (void)r; }
}

// TCP/UDP host port-forward. host_ip_be / guest_ip_be are IPv4 in network byte order; ports host
// order. Returns 0 on success, <0 on error.
int aemu_slirp_add_hostfwd(AemuSlirp *a, int is_udp, uint32_t host_ip_be, int host_port,
                           uint32_t guest_ip_be, int guest_port) {
    struct in_addr haddr, gaddr;
    haddr.s_addr = host_ip_be;
    gaddr.s_addr = guest_ip_be;
    return slirp_add_hostfwd(a->slirp, is_udp, haddr, host_port, gaddr, guest_port);
}

// Fill the poll set. fds[0] is reserved for the wake pipe; libslirp fds follow. Writes the slirp
// timeout hint (ms; UINT32_MAX == infinite) through *timeout. Caller must hold the lock.
void aemu_slirp_fill(AemuSlirp *a, uint32_t *timeout) {
    a->fds[0].fd = a->wake_rd;
    a->fds[0].events = POLLIN;
    a->fds[0].revents = 0;
    a->nfds = 1;
    *timeout = UINT32_MAX;
    slirp_pollfds_fill_socket(a->slirp, timeout, add_poll_cb, a);
}

struct pollfd *aemu_slirp_fds(AemuSlirp *a) { return a->fds; }
int aemu_slirp_nfds(AemuSlirp *a) { return a->nfds; }

// Report poll results back to slirp + fire any expired timers + drain the wake pipe. Caller must
// hold the lock. `poll_error` is 1 if the poll() syscall itself failed.
void aemu_slirp_poll_result(AemuSlirp *a, int poll_error) {
    // Drain the wake pipe (fds[0]).
    if (a->fds[0].revents & POLLIN) {
        char tmp[64];
        while (read(a->wake_rd, tmp, sizeof(tmp)) > 0) { /* drain */ }
    }
    slirp_pollfds_poll(a->slirp, poll_error, get_revents_cb, a);

    // Fire expired v1 timers.
    int64_t t = now_ms();
    for (AemuTimer *tm = a->timers; tm; tm = tm->next) {
        if (tm->active && tm->expire_ms != 0 && t >= tm->expire_ms) {
            tm->active = 0;
            if (tm->cb) tm->cb(tm->cb_opaque);
        }
    }
}

// Poke the wake pipe (no slirp state touched — safe to call without the lock).
void aemu_slirp_kick(AemuSlirp *a) {
    if (a && a->wake_wr >= 0) { char b = 'k'; ssize_t r = write(a->wake_wr, &b, 1); (void)r; }
}

void aemu_slirp_cleanup(AemuSlirp *a) {
    if (!a) return;
    if (a->slirp) slirp_cleanup(a->slirp);
    if (a->wake_rd >= 0) close(a->wake_rd);
    if (a->wake_wr >= 0) close(a->wake_wr);
    AemuTimer *tm = a->timers;
    while (tm) { AemuTimer *n = tm->next; free(tm); tm = n; }
    free(a);
}
