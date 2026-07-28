// The live surface — framebuffer + tap/swipe + the control bar. Lifted out of
// the old standalone Running screen. Two changes beyond the move:
//  - the framebuffer poll is scoped to THIS tab's lifetime (mount/unmount), so
//    reading Settings doesn't leave a screenshot poll running;
//  - a stopped VM gets a real empty state with a Start affordance instead of a
//    spinner claiming it's booting (the old screen was only reachable when up).
//
// The controls sit in a full-width menu-bar strip directly under the tabbar
// (they used to be a 330px right rail, which cost the framebuffer a third of the
// window). The guest picture is the point of this screen, so it now gets the
// whole width and the controls get one row of height.
//
// Input capture has no toggle: clicking the screen hands the mouse AND the keyboard
// to the guest, exactly like clicking into any other app. The blue pill on the frame
// is the armed indicator and the only place the exit (Cmd+Esc) is written down.
// Release also happens on window blur, on clicking a field in the bar, when the VM
// stops, and on unmount — each of those paths must release every held key.

import { useCallback, useEffect, useRef, useState } from 'react'
import * as ipc from '../../lib/ipc'
import { Channel } from '../../lib/ipc'
import { evdevForCode, isHostReservedChord } from '../../lib/keymap'
import type { RenderMode, VmInstance } from '../../lib/types'

// Android-relevant hardware keys the control plane accepts by name.
const HW_KEYS: { code: string; label: string }[] = [
    { code: 'HOME', label: 'Home' },
    { code: 'BACK', label: 'Back' },
    { code: 'MENU', label: 'Menu' },
    { code: 'ENTER', label: 'Enter' },
    { code: 'POWER', label: 'Power' },
    { code: 'VOLUP', label: 'Vol +' },
    { code: 'VOLDOWN', label: 'Vol −' },
]

// evdev modifier codes. Legacy control planes (no keydown/keyup) can't hold these,
// so the fallback path drops them rather than firing a press+release nobody wanted.
const MODIFIER_CODES = new Set([42, 54, 29, 97, 56, 100, 125, 126, 58])

const EDITABLE_TAGS = /^(INPUT|TEXTAREA|SELECT)$/

export function DisplayTab({
    id,
    inst,
    renderMode,
    busy,
    note,
    onFlash,
    onError,
    onStart,
    onGoToLogs,
}: {
    id: string
    inst: VmInstance | null
    renderMode: RenderMode | null
    busy: boolean
    note: string | null
    onFlash: (msg: string) => void
    onError: (msg: string) => void
    onStart: () => void
    onGoToLogs: () => void
}) {
    const [hasFrame, setHasFrame] = useState(false)
    const [text, setText] = useState('')
    const [pkg, setPkg] = useState('')
    const [capture, setCapture] = useState(false)

    const canvasRef = useRef<HTMLCanvasElement | null>(null)
    const ctxRef = useRef<CanvasRenderingContext2D | null>(null)
    // Frame-delivery counters, surfaced in the placeholder so a stall is diagnosable
    // from the screen instead of from a terminal the user isn't looking at.
    const rx = useRef(0)
    const decodeWarned = useRef(false)
    const dragStart = useRef<{ x: number; y: number } | null>(null)
    // The newest decoded frame, kept so the paint can be retried once a canvas exists.
    const pending = useRef<{
        width: number
        height: number
        pixels: Uint8ClampedArray<ArrayBuffer>
    } | null>(null)

    // Live-keyboard state. `held` is what the GUEST currently has pressed — the
    // release ledger, so a blur/disarm/unmount can never leave a stuck modifier.
    const held = useRef<Set<number>>(new Set())
    // Control-plane writes are one socket round-trip each, so two concurrent sends
    // can land out of order — a modifier's press arriving AFTER the key it modifies
    // turns Cmd+A into a plain "a". Chain them.
    const queue = useRef<Promise<unknown>>(Promise.resolve())
    // Set when the running VM's control plane predates keydown/keyup (an old
    // aemulator-hvf binary): fall back to atomic press+release, no chords.
    const legacyKeys = useRef(false)
    const reportedKeyError = useRef(false)

    const state = inst?.state ?? 'Stopped'
    const running = state === 'Ready' || state === 'Booting' || state === 'Provisioning'
    const ready = state === 'Ready'
    const dispW = inst?.display.width || 1024
    const dispH = inst?.display.height || 768

    // `gen` is the guest's present counter — gpu.rs `flush_3d` bumps it on BOTH the
    // zero-copy and readback paths, so gen === 0 means the guest has not presented a
    // single frame yet (still booting), NOT that this render mode skips the counter.
    // Distinguish that from "frames are arriving": the studio's poll is gen-gated, so
    // a stuck gen also means no new screenshot is being fetched.
    const gen = inst?.display.gen ?? 0
    const nonzero = inst?.display.nonzero ?? 0
    const noFrameYet = gen === 0

    // Live framebuffer stream: raw RGBA frames at ~60fps, blitted straight into a
    // canvas. Only while a running VM's Display tab is mounted.
    //
    // Frames are painted imperatively rather than through React state on purpose —
    // at 60fps a setState per frame would re-render this whole (large) component 60
    // times a second to change pixels React doesn't own. `hasFrame` flips once, just
    // to swap the placeholder out.
    // Blit `pending.current` into the canvas. Safe to call when the canvas does not exist
    // yet or has no 2D context — the frame stays pending and the next call paints it.
    // Painting into a hidden canvas is fine: the backing store is independent of layout,
    // so the picture is already there when it is unhidden.
    const paint = useCallback(() => {
        const f = pending.current
        const canvas = canvasRef.current
        if (!f || !canvas) return
        // Resize the backing store only on an actual geometry change; assigning
        // width/height clears the canvas, so doing it per frame would flicker.
        if (canvas.width !== f.width || canvas.height !== f.height) {
            canvas.width = f.width
            canvas.height = f.height
            // A resize invalidates nothing about the context, but the cached one may
            // belong to a previous canvas element after a remount.
            ctxRef.current = null
        }
        const ctx = ctxRef.current ?? canvas.getContext('2d', { alpha: false })
        if (!ctx) return
        ctxRef.current = ctx
        ctx.putImageData(new ImageData(f.pixels, f.width, f.height), 0, 0)
    }, [])

    // Paint whatever is pending once the canvas is in the DOM. Covers the first frame
    // (which may decode before this component's canvas has been laid out) and a remount.
    useEffect(() => {
        if (hasFrame) paint()
    }, [hasFrame, paint])

    useEffect(() => {
        if (!running) {
            setHasFrame(false)
            pending.current = null
            ctxRef.current = null
            return
        }
        const chan = new Channel<ArrayBuffer>()
        chan.onmessage = (buf) => {
            // Count every message BEFORE decoding. This distinguishes the two
            // failure modes that look identical on screen: nothing arriving over
            // the IPC channel at all, versus messages arriving that we then fail
            // to decode. Chasing the wrong one of those cost hours.
            rx.current += 1
            const f = ipc.decodeFrame(buf)
            if (!f) {
                if (!decodeWarned.current) {
                    decodeWarned.current = true
                    const len =
                        buf && typeof (buf as ArrayBuffer).byteLength === 'number'
                            ? `${(buf as ArrayBuffer).byteLength} bytes`
                            : `type ${typeof buf}`
                    onError(
                        `live view: received a frame message but could not decode it (${len}). ` +
                            `Expected an 8-byte header + w*h*4 RGBA.`,
                    )
                }
                return
            }
            // Hold the newest frame so a paint that cannot happen YET is deferred rather
            // than dropped.
            //
            // Retained as a VIEW, not a copy. A frame is ~3 MB, so copying each one would
            // burn ~180 MB/s of allocation + GC at 60fps for no benefit: Tauri hands every
            // channel message its own ArrayBuffer (payloads this size travel the
            // `plugin:__TAURI_CHANNEL__|fetch` path and end in `response.arrayBuffer()`),
            // so nothing overwrites it. Only the newest is referenced, so at most one
            // frame's buffer is held alive.
            pending.current = f
            paint()
            // Flip on a successful DECODE, not on a successful paint. `hasFrame` means "a
            // frame reached this window", which is what the placeholder is deciding about;
            // tying it to the paint is what deadlocked (paint needs the canvas, the canvas
            // was gated on hasFrame). It also kept the UI insisting no frame had arrived
            // while its own counter showed 305 — the state now cannot contradict the counter.
            setHasFrame(true)
        }
        ipc.streamFramebuffer(id, chan).catch((e) => onError(String(e)))
        return () => {
            ipc.stopFramebuffer(id).catch(() => {})
        }
    }, [id, running, paint, onError])

    // ---- Live keyboard capture ----

    const enqueue = useCallback((op: () => Promise<unknown>) => {
        const next = queue.current.then(op, op)
        // Keep the chain alive after a rejection (one bad key must not wedge typing).
        queue.current = next.catch(() => {})
        return next
    }, [])

    // Release everything the guest still has pressed. Called on disarm, window blur,
    // VM stop and unmount — a dropped keyup is a modifier stuck down inside Android.
    const releaseAll = useCallback(() => {
        const codes = [...held.current]
        held.current.clear()
        for (const c of codes) {
            enqueue(() => ipc.sendKeyUp(id, String(c))).catch(() => {})
        }
    }, [enqueue, id])

    const onKeyError = useCallback(
        (e: unknown) => {
            const msg = String(e)
            // The verb is missing when the VM was launched from an aemulator-hvf build
            // that predates keydown/keyup — degrade instead of dying.
            if (msg.includes('unknown verb')) {
                if (!legacyKeys.current) {
                    legacyKeys.current = true
                    held.current.clear()
                    onError(
                        "This VM's control plane has no keydown/keyup — restart the emulator against a current aemulator-hvf build for modifier chords. Typing still works (press+release only).",
                    )
                }
                return
            }
            if (!reportedKeyError.current) {
                reportedKeyError.current = true
                onError(msg)
            }
        },
        [onError],
    )

    // A stopped VM can't take keys; drop capture so the badge never lies.
    useEffect(() => {
        if (!ready) setCapture(false)
    }, [ready])

    useEffect(() => {
        if (!capture || !ready) return

        function forwardable(ev: KeyboardEvent): number | null {
            // The control bar's own fields keep their keystrokes — capture is for the
            // guest, not for stealing the package name you're typing.
            const t = ev.target as HTMLElement | null
            if (t && (EDITABLE_TAGS.test(t.tagName) || t.isContentEditable)) return null
            if (isHostReservedChord(ev)) return null
            return evdevForCode(ev.code)
        }

        function onKeyDown(ev: KeyboardEvent) {
            // The release valve, and with no toggle button it is the primary one —
            // hence the pill advertising it. macOS leaves this chord alone (Force Quit
            // is Cmd+Opt+Esc), so it reaches us even with Command held.
            if (ev.metaKey && ev.code === 'Escape') {
                ev.preventDefault()
                setCapture(false)
                onFlash('input released')
                return
            }
            const code = forwardable(ev)
            if (code == null) return
            ev.preventDefault()
            ev.stopPropagation()
            // Autorepeat: the key is already down in the guest, whose own input reader
            // generates the repeats. Re-sending press events would double them.
            if (ev.repeat) return
            if (legacyKeys.current) {
                if (MODIFIER_CODES.has(code)) return
                enqueue(() => ipc.sendKey(id, String(code))).catch(onKeyError)
                return
            }
            held.current.add(code)
            enqueue(() => ipc.sendKeyDown(id, String(code))).catch(onKeyError)
        }

        function onKeyUp(ev: KeyboardEvent) {
            const code = forwardable(ev)
            if (code == null) return
            ev.preventDefault()
            ev.stopPropagation()
            if (legacyKeys.current) return
            // macOS suppresses the keyup for a letter released while Command is held,
            // so a Cmd chord otherwise leaves that letter down. Releasing Command
            // releases the whole held set, which is also what the guest expects.
            if (code === 125 || code === 126) {
                releaseAll()
                return
            }
            if (!held.current.delete(code)) return
            enqueue(() => ipc.sendKeyUp(id, String(code))).catch(onKeyError)
        }

        // Losing window focus means we stop seeing keyups — disarm rather than risk a
        // half-held chord in the guest.
        function onWindowBlur() {
            setCapture(false)
        }

        // Clicking one of the bar's fields hands the keyboard back to the host. The
        // key handlers already skip events aimed at a text field, so this is about
        // honesty: the pill must not claim the guest has keys that are going elsewhere.
        function onDocMouseDown(ev: MouseEvent) {
            const t = ev.target as HTMLElement | null
            if (t && (EDITABLE_TAGS.test(t.tagName) || t.isContentEditable)) setCapture(false)
        }

        // Capture phase: beat React's own delegated handlers and any focused control.
        window.addEventListener('keydown', onKeyDown, true)
        window.addEventListener('keyup', onKeyUp, true)
        window.addEventListener('blur', onWindowBlur)
        document.addEventListener('mousedown', onDocMouseDown, true)
        return () => {
            window.removeEventListener('keydown', onKeyDown, true)
            window.removeEventListener('keyup', onKeyUp, true)
            window.removeEventListener('blur', onWindowBlur)
            document.removeEventListener('mousedown', onDocMouseDown, true)
            releaseAll()
        }
    }, [capture, ready, id, enqueue, releaseAll, onKeyError, onFlash])

    // The canvas fills the frame with `object-fit: contain`, so its element box is
    // larger than the rendered picture and the difference is letterbox bars. Mapping
    // through getBoundingClientRect() alone would therefore skew every tap by the bar
    // size — derive the content rect and map through that instead. Returns null for
    // clicks that land on a bar (no phantom edge taps).
    //
    // Geometry comes from the canvas's own backing store, which is set from each
    // frame's header, rather than from the polled `inst.display`: the frame is the
    // authoritative source and the 1.5s status poll can lag a resolution change.
    function toDisplayCoords(ev: React.MouseEvent): { x: number; y: number } | null {
        const el = canvasRef.current
        if (!el) return null
        const gw = el.width || dispW
        const gh = el.height || dispH
        const r = el.getBoundingClientRect()
        if (r.width === 0 || r.height === 0) return null
        const scale = Math.min(r.width / gw, r.height / gh)
        if (!(scale > 0)) return null
        const contentW = gw * scale
        const contentH = gh * scale
        const originX = r.left + (r.width - contentW) / 2
        const originY = r.top + (r.height - contentH) / 2
        const x = Math.round((ev.clientX - originX) / scale)
        const y = Math.round((ev.clientY - originY) / scale)
        if (x < 0 || y < 0 || x >= gw || y >= gh) return null
        return { x, y }
    }

    async function guard(fn: () => Promise<unknown>, label: string) {
        try {
            await fn()
            onFlash(label)
        } catch (e) {
            onError(String(e))
        }
    }

    function onDown(ev: React.MouseEvent) {
        if (!ready) return
        // Clicking into the screen IS the capture gesture — there is no toggle. The
        // same click still taps the guest, so arming costs the user nothing.
        if (!capture) {
            // Drop focus from whatever control had it, or the first keystroke would
            // re-activate a focused button (Space/Enter) instead of reaching Android.
            ;(document.activeElement as HTMLElement | null)?.blur()
            setCapture(true)
        }
        dragStart.current = toDisplayCoords(ev)
    }

    function onUp(ev: React.MouseEvent) {
        if (!ready) return
        const start = dragStart.current
        const end = toDisplayCoords(ev)
        dragStart.current = null
        if (!start || !end) return
        const dist = Math.hypot(end.x - start.x, end.y - start.y)
        if (dist < 12) {
            guard(() => ipc.sendTap(id, end.x, end.y), `tap ${end.x},${end.y}`)
        } else {
            guard(() => ipc.sendSwipe(id, start.x, start.y, end.x, end.y, 300), 'swipe →')
        }
    }

    return (
        <div className="running">
            {/* Menu-bar strip: one row, edge to edge, above the stage. Groups are
                separated by hairlines rather than stacked labels so the whole
                control surface costs ~34px of height instead of a column. */}
            <div className="display-bar">
                <div className="dbar-group">
                    {HW_KEYS.map((k) => (
                        <button
                            key={k.code}
                            className="btn dbar-btn"
                            disabled={!ready}
                            onClick={() => guard(() => ipc.sendKey(id, k.code), `key ${k.label}`)}
                        >
                            {k.label}
                        </button>
                    ))}
                </div>

                <span className="dbar-sep" />

                <div className="dbar-group grow">
                    <input
                        className="dbar-input"
                        value={text}
                        onChange={(e) => setText(e.target.value)}
                        placeholder="text to inject"
                        disabled={!ready}
                        onKeyDown={(e) => {
                            if (e.key !== 'Enter' || !ready || !text) return
                            guard(async () => {
                                await ipc.sendText(id, text)
                                setText('')
                            }, 'text sent')
                        }}
                    />
                    <button
                        className="btn primary dbar-btn"
                        disabled={!ready || !text}
                        onClick={() =>
                            guard(async () => {
                                await ipc.sendText(id, text)
                                setText('')
                            }, 'text sent')
                        }
                    >
                        Send
                    </button>
                </div>

                <span className="dbar-sep" />

                <div className="dbar-group grow">
                    <input
                        className="dbar-input"
                        value={pkg}
                        onChange={(e) => setPkg(e.target.value)}
                        placeholder="com.android.settings"
                        disabled={!ready}
                        onKeyDown={(e) => {
                            if (e.key !== 'Enter' || !ready || !pkg) return
                            guard(() => ipc.launchPackage(id, pkg), 'launch sent')
                        }}
                    />
                    <button
                        className="btn dbar-btn"
                        disabled={!ready || !pkg}
                        title="Launch a package (best-effort, over the guest console)"
                        onClick={() => guard(() => ipc.launchPackage(id, pkg), 'launch sent')}
                    >
                        Launch
                    </button>
                </div>

                <span className="dbar-sep" />

                <div className="dbar-group">
                    <button
                        className="btn dbar-btn"
                        disabled={!ready}
                        onClick={() =>
                            // The live view already streams at frame cadence, so painting a
                            // one-shot capture into it would be overwritten ~16ms later.
                            // Save it instead — the PNG the `screenshot` verb encodes is
                            // genuinely useful, and nothing else in the app could keep one.
                            guard(async () => {
                                const url = await ipc.screenshot(id)
                                const a = document.createElement('a')
                                a.href = url
                                a.download = `aemulator-${id.slice(0, 8)}-${Date.now()}.png`
                                a.click()
                            }, 'screenshot saved')
                        }
                    >
                        Screenshot
                    </button>
                    <button
                        className="btn dbar-btn"
                        disabled={!ready}
                        onClick={() => guard(() => ipc.waitStable(id, 8000), 'settled')}
                    >
                        Wait stable
                    </button>
                </div>
            </div>

            <div className="run-stage">
                <div className="device-frame">
                    {/* Ordered ahead of `frame` so an all-black first frame can't mask
                        the "nothing presented yet" state. */}
                    {/* `hasFrame` is checked FIRST: a delivered frame is ground truth, while
                        `gen` arrives via the 1.5s status poll and can lag it. Gating the canvas
                        behind `gen` meant a live picture could sit hidden behind a spinner. */}
                    {/* The canvas is mounted UNCONDITIONALLY and merely hidden until the first
                        frame paints. It must never be conditional on `hasFrame`, which is the
                        deadlock this code shipped with:

                          canvas rendered only when hasFrame
                            → canvasRef.current is null
                              → the frame handler's `if (!canvas) return` fires
                                → setHasFrame(true) is never reached
                                  → the canvas never mounts

                        Frames arrived the whole time (the placeholder's own "frames delivered"
                        counter read 305) and every one was dropped one line before the paint,
                        silently, because `rx` is counted before the bail-out. The screen showed
                        "no frame has arrived here" while 305 had.

                        `hidden` (→ display:none) rather than unmounting: a canvas's backing store
                        and `putImageData` are independent of layout, so the ref stays valid and the
                        first frame paints into a canvas that is already there. */}
                    <canvas
                        ref={canvasRef}
                        className="fb"
                        hidden={!hasFrame}
                        onMouseDown={onDown}
                        onMouseUp={onUp}
                        style={{ cursor: ready ? 'crosshair' : 'default' }}
                    />
                    {hasFrame ? null : running && noFrameYet ? (
                        <div className="fb-placeholder">
                            <div className="spinner" />
                            <div style={{ maxWidth: 470, lineHeight: 1.55 }}>
                                <b>The guest hasn't presented a frame yet.</b>
                                <br />
                                Android is still coming up — a first boot on a freshly
                                provisioned disk takes several minutes. The picture appears
                                here as soon as the guest presents.
                            </div>
                            <div className="fb-sub">
                                scanout {dispW}×{dispH} · gen {gen} · nonzero {nonzero} — gen
                                counts guest presents, so it stays 0 until the first frame
                            </div>
                            <div className="fb-sub">
                                pipeline{' '}
                                {renderMode === 'Gpu3d'
                                    ? 'Metal GPU (gfxstream, present readback → in-app)'
                                    : renderMode === 'SwRender'
                                      ? 'SwRender (guest-CPU SwiftShader)'
                                      : renderMode === 'Headless'
                                        ? 'Headless — no render pipeline, the framebuffer stays blank'
                                        : '—'}
                            </div>
                            <button className="btn" onClick={onGoToLogs}>
                                Open Logs
                            </button>
                        </div>
                    ) : !running ? (
                        <div className="fb-placeholder">
                            <div className="fb-idle-mark">▦</div>
                            <div>
                                {state === 'Crashed'
                                    ? 'The VM exited unexpectedly — see the Logs tab.'
                                    : 'This emulator is not running.'}
                            </div>
                            <button className="btn primary" disabled={busy} onClick={onStart}>
                                Start emulator
                            </button>
                        </div>
                    ) : (
                        // Reached when the guest IS presenting (gen > 0) but no frame has
                        // reached this component. That is a delivery problem, not a boot
                        // problem, so say which — "waiting for first frame" sent me hunting
                        // the guest when the guest was fine.
                        <div className="fb-placeholder">
                            <div className="spinner" />
                            <div style={{ maxWidth: 470, lineHeight: 1.55 }}>
                                {ready ? (
                                    <>
                                        <b>The guest is presenting, but no frame has arrived here.</b>
                                        <br />
                                        The VM is rendering (gen is advancing) and the capture path
                                        is fine, so this is the frame stream between the VMM and
                                        this window. Most likely the running VM predates the raw-
                                        frame path — Stop and Start it to relaunch on the current
                                        emulator binary.
                                    </>
                                ) : (
                                    'booting Android — the picture appears once the guest renders'
                                )}
                            </div>
                            <div className="fb-sub">
                                {dispW}×{dispH} · gen {inst?.display.gen ?? 0} · nonzero{' '}
                                {inst?.display.nonzero ?? 0} · frames delivered to this
                                window: {rx.current}
                            </div>
                            <button className="btn" onClick={onGoToLogs}>
                                Open Logs
                            </button>
                        </div>
                    )}
                    {/* The only capture affordance: armed by clicking the screen, so the
                        pill is how the user knows input is going to the guest — and the
                        only place the way out is written down. */}
                    {capture && (
                        <div className="fb-capture-chip">
                            ⌨ mouse + keyboard captured · <b>Cmd+Esc</b> to exit
                        </div>
                    )}
                    {note && <div className="fb-toast">{note}</div>}
                </div>
            </div>
        </div>
    )
}
