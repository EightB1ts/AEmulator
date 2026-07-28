// The per-emulator hub — the single surface for ONE emulator. Replaces the
// global Devices page (whose first act was asking "which emulator?" via a
// dropdown) and absorbs the old standalone Running view.
//
// State that must survive a tab switch lives here: the profile, the polled
// instance status, and the log ring. The framebuffer poll deliberately does NOT
// — it starts/stops with the Display tab so we aren't screenshot-polling a VM
// while the user reads Settings.

import { useEffect, useRef, useState } from 'react'
import * as ipc from '../lib/ipc'
import type { EmulatorProfile, VmInstance, VmLogEvent } from '../lib/types'
import { DisplayTab } from '../components/instance/DisplayTab'
import { HardwareTab } from '../components/instance/HardwareTab'
import { SettingsTab } from '../components/instance/SettingsTab'
import { StorageTab } from '../components/instance/StorageTab'
import { LogsTab } from '../components/instance/LogsTab'

export type InstanceTab = 'display' | 'hardware' | 'settings' | 'storage' | 'logs'

const TABS: { key: InstanceTab; label: string }[] = [
    { key: 'display', label: 'Display' },
    { key: 'hardware', label: 'Hardware' },
    { key: 'settings', label: 'Settings' },
    { key: 'storage', label: 'Storage' },
    { key: 'logs', label: 'Logs' },
]

const LOG_CAP = 1000

export function Instance({
    id,
    tab,
    onBack,
    onTab,
    onDeleted,
}: {
    id: string
    tab: InstanceTab
    onBack: () => void
    onTab: (t: InstanceTab) => void
    onDeleted: () => void
}) {
    const [profile, setProfile] = useState<EmulatorProfile | null>(null)
    const [inst, setInst] = useState<VmInstance | null>(null)
    const [logs, setLogs] = useState<VmLogEvent[]>([])
    const [busy, setBusy] = useState(false)
    const [note, setNote] = useState<string | null>(null)
    const [err, setErr] = useState<string | null>(null)
    const noteTimer = useRef<number | null>(null)

    // Resolve the profile + prime the log ring.
    useEffect(() => {
        ipc.listEmulators()
            .then((ps) => setProfile(ps.find((p) => p.id === id) ?? null))
            .catch((e) => setErr(String(e)))
        ipc.getLogs(id).then(setLogs).catch(() => {})
    }, [id])

    // Poll instance status (state, display geometry, adb port).
    useEffect(() => {
        let alive = true
        const tick = () =>
            ipc
                .getStatus(id)
                .then((i) => alive && setInst(i))
                .catch(() => {})
        tick()
        const t = setInterval(tick, 1500)
        return () => {
            alive = false
            clearInterval(t)
        }
    }, [id])

    // Backend-reported failures (spawn/boot AND live-view frame-stream errors) surface
    // as the error bar. Without this a stalled frame loop was only visible in the dev
    // server's stdout, which is not where anyone is looking.
    useEffect(() => {
        const un = ipc.onVmError((e) => {
            if (e.id !== id) return
            setErr(e.message)
        })
        return () => {
            un.then((f) => f())
        }
    }, [id])

    // Log tail lives at the hub so switching tabs never drops the buffer.
    useEffect(() => {
        const un = ipc.onVmLog((e) => {
            if (e.id !== id) return
            setLogs((prev) => {
                const next = [...prev, e]
                return next.length > LOG_CAP ? next.slice(-LOG_CAP) : next
            })
        })
        return () => {
            un.then((f) => f())
        }
    }, [id])

    useEffect(
        () => () => {
            if (noteTimer.current) window.clearTimeout(noteTimer.current)
        },
        [],
    )

    function flash(msg: string) {
        setNote(msg)
        if (noteTimer.current) window.clearTimeout(noteTimer.current)
        noteTimer.current = window.setTimeout(() => setNote(null), 1400)
    }

    const state = inst?.state ?? 'Stopped'
    const running = state === 'Ready' || state === 'Booting' || state === 'Provisioning'

    async function start() {
        setBusy(true)
        setErr(null)
        try {
            const i = await ipc.startEmulator(id)
            setInst(i)
            onTab('display') // watch it boot
        } catch (e) {
            setErr(String(e))
        } finally {
            setBusy(false)
        }
    }

    async function stop() {
        setBusy(true)
        try {
            await ipc.stopEmulator(id)
            setInst(await ipc.getStatus(id))
        } catch (e) {
            setErr(String(e))
        } finally {
            setBusy(false)
        }
    }

    return (
        <>
            {/* "deep" = the whole toolbar drags; Tauri's drag script auto-excludes
                clickable children (button/input/select/label/a), so the buttons and
                the adb hint below keep working. */}
            <div className="toolbar instance-bar" data-tauri-drag-region="deep">
                <button className="btn" onClick={onBack}>
                    ← Emulators
                </button>
                <h1 style={{ marginLeft: 4 }}>{profile?.name ?? id}</h1>
                <span className={`pill ${state.toLowerCase()}`} style={{ marginLeft: 6 }}>
                    <span className="dot" />
                    {state}
                </span>
                <div className="spacer" />
                <div className="actions">
                    {running ? (
                        <button className="btn danger" disabled={busy} onClick={stop}>
                            Stop
                        </button>
                    ) : (
                        <button className="btn primary" disabled={busy} onClick={start}>
                            Start
                        </button>
                    )}
                </div>
            </div>

            <div className="tabbar">
                {TABS.map((t) => (
                    <button
                        key={t.key}
                        className={`tab${tab === t.key ? ' active' : ''}`}
                        onClick={() => onTab(t.key)}
                    >
                        {t.label}
                        {t.key === 'logs' && logs.length > 0 && (
                            <span className="tab-count">{logs.length}</span>
                        )}
                    </button>
                ))}
            </div>

            {err && (
                <div className="run-error" onClick={() => setErr(null)}>
                    {err} · click to dismiss
                </div>
            )}

            {tab === 'display' && (
                <DisplayTab
                    id={id}
                    inst={inst}
                    renderMode={profile?.render_mode ?? null}
                    busy={busy}
                    note={note}
                    onFlash={flash}
                    onError={setErr}
                    onStart={start}
                    onGoToLogs={() => onTab('logs')}
                />
            )}
            {tab === 'hardware' && (
                <HardwareTab id={id} onError={setErr} onGoToDisplay={() => onTab('display')} />
            )}
            {tab === 'settings' && profile && (
                <SettingsTab
                    profile={profile}
                    running={running}
                    onSaved={(p) => {
                        setProfile(p)
                        flash('settings saved')
                    }}
                    onDeleted={onDeleted}
                    onError={setErr}
                />
            )}
            {tab === 'storage' && <StorageTab id={id} onError={setErr} />}
            {tab === 'logs' && <LogsTab logs={logs} />}
        </>
    )
}
