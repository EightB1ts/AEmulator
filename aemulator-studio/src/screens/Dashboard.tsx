import { useEffect, useMemo, useState } from 'react'
import * as ipc from '../lib/ipc'
import type { EmulatorProfile, HostCaps, VmInstance, VmState } from '../lib/types'
import { EmulatorCard } from '../components/EmulatorCard'
import { fmtMb } from '../lib/format'

export function Dashboard({
    onCreate,
    onOpenInstance,
}: {
    onCreate: () => void
    onOpenInstance: (id: string) => void
}) {
    const [profiles, setProfiles] = useState<EmulatorProfile[]>([])
    const [instances, setInstances] = useState<VmInstance[]>([])
    const [caps, setCaps] = useState<HostCaps | null>(null)
    const [busy, setBusy] = useState<string | null>(null)
    const [err, setErr] = useState<string | null>(null)

    async function refresh() {
        try {
            const [p, i, c] = await Promise.all([
                ipc.listEmulators(),
                ipc.listInstances(),
                ipc.hostCapabilities(),
            ])
            setProfiles(p)
            setInstances(i)
            setCaps(c)
            setErr(null)
        } catch (e) {
            setErr(String(e))
        }
    }

    useEffect(() => {
        refresh()
        const un = ipc.onVmState(() => refresh())
        // Poll host capacity + instances so free-RAM + boot state stay live.
        const t = setInterval(refresh, 2500)
        return () => {
            clearInterval(t)
            un.then((f) => f())
        }
    }, [])

    const stateById = useMemo(() => {
        const m = new Map<string, VmState>()
        for (const inst of instances) m.set(inst.id, inst.state)
        return m
    }, [instances])

    const usedMb = useMemo(
        () =>
            instances
                .filter((i) => i.state !== 'Stopped' && i.state !== 'Crashed')
                .reduce((sum, i) => {
                    const p = profiles.find((x) => x.id === i.id)
                    return sum + (p?.mem_mb ?? 0)
                }, 0),
        [instances, profiles],
    )

    async function start(id: string) {
        setBusy(id)
        try {
            await ipc.startEmulator(id)
            await refresh()
            onOpenInstance(id) // jump to the live view to watch it boot
        } catch (e) {
            setErr(String(e))
        } finally {
            setBusy(null)
        }
    }

    async function stop(id: string) {
        setBusy(id)
        try {
            await ipc.stopEmulator(id)
            await refresh()
        } catch (e) {
            setErr(String(e))
        } finally {
            setBusy(null)
        }
    }

    async function remove(id: string) {
        const p = profiles.find((x) => x.id === id)
        if (!confirm(`Delete "${p?.name ?? id}"? Its provisioned disk image is removed too.`))
            return
        setBusy(id)
        try {
            await ipc.deleteEmulator(id)
            await refresh()
        } catch (e) {
            setErr(String(e))
        } finally {
            setBusy(null)
        }
    }

    const capPct = caps ? Math.min(100, (usedMb / caps.total_ram_mb) * 100) : 0

    return (
        <>
            <div className="toolbar" data-tauri-drag-region="deep">
                <h1>Emulators</h1>
                <div className="spacer" />
                <div className="actions">
                    <button className="btn" onClick={refresh}>
                        Refresh
                    </button>
                    <button className="btn primary" onClick={onCreate}>
                        + New Emulator
                    </button>
                </div>
            </div>

            <div className="content">
                {caps && (
                    <div className="capacity">
                        <div className="stat">
                            <b>{fmtMb(usedMb)}</b>
                            <span>RAM in use</span>
                        </div>
                        <div className="meter">
                            <span style={{ width: `${capPct}%` }} />
                        </div>
                        <div className="stat">
                            <b>{fmtMb(caps.free_ram_mb)}</b>
                            <span>Host free</span>
                        </div>
                        <div className="stat">
                            <b>{caps.max_vms}</b>
                            <span>Max VMs</span>
                        </div>
                        <div className="stat">
                            <b>{caps.binary_signed ? 'Signed' : 'Unsigned'}</b>
                            <span>hvf binary</span>
                        </div>
                        <div className="stat">
                            <b>{caps.libslirp ? 'Yes' : 'No'}</b>
                            <span>libslirp</span>
                        </div>
                    </div>
                )}

                {err && (
                    <p className="section-note" style={{ color: 'var(--red)' }}>
                        {err}
                    </p>
                )}

                <div className="grid">
                    {profiles.map((p) => (
                        <EmulatorCard
                            key={p.id}
                            profile={p}
                            state={stateById.get(p.id) ?? 'Stopped'}
                            busy={busy === p.id}
                            onStart={() => start(p.id)}
                            onStop={() => stop(p.id)}
                            onOpen={() => onOpenInstance(p.id)}
                            onDelete={() => remove(p.id)}
                        />
                    ))}

                    <button className="card new-tile" onClick={onCreate}>
                        <div className="plus">+</div>
                        <div>New Emulator</div>
                        <div style={{ fontSize: 11, color: 'var(--text-faint)' }}>
                            profile + device setup
                        </div>
                    </button>
                </div>

                {profiles.length === 0 && !err && (
                    <div className="empty">
                        <h2>No emulators yet</h2>
                        <p>The backend returned an empty profile list.</p>
                    </div>
                )}
            </div>
        </>
    )
}
