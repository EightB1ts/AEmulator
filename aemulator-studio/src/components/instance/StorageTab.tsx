// This instance's disk — the per-emulator half of the old global Storage page.
// The global page keeps the base-image catalog + a cross-instance roll-up; the
// COW meter, partition breakdown and snapshots belong to the instance.

import { useEffect, useState } from 'react'
import * as ipc from '../../lib/ipc'
import type { StorageVolume } from '../../lib/types'
import { fmtBytes } from '../../lib/format'

export function StorageTab({ id, onError }: { id: string; onError: (msg: string) => void }) {
    const [vol, setVol] = useState<StorageVolume | null>(null)
    const [snapMsg, setSnapMsg] = useState<string | null>(null)

    useEffect(() => {
        let alive = true
        const refresh = () =>
            ipc
                .listStorage()
                .then((vs) => alive && setVol(vs.find((v) => v.emulator_id === id) ?? null))
                .catch((e) => onError(String(e)))
        refresh()
        const t = setInterval(refresh, 4000)
        return () => {
            alive = false
            clearInterval(t)
        }
    }, [id])

    async function trySnapshot() {
        try {
            await ipc.createSnapshot(id, `snap-${Date.now()}`)
        } catch (e) {
            // Expected: Phase-2 placeholder. Surface the honest message.
            setSnapMsg(String(e))
        }
    }

    if (!vol) {
        return (
            <div className="content">
                <div className="empty">
                    <h2>No disk provisioned yet</h2>
                    <p>
                        The copy-on-write clone is created on first start. Start this emulator to
                        provision its instance directory.
                    </p>
                </div>
            </div>
        )
    }

    const saved = vol.logical_bytes - vol.on_disk_bytes
    const pct = vol.logical_bytes > 0 ? (vol.on_disk_bytes / vol.logical_bytes) * 100 : 0

    return (
        <div className="content">
            <p className="section-note">
                The instance image is an APFS clonefile (copy-on-write) clone of the base image, so
                on-disk bytes stay far below logical size until the VM writes.
            </p>

            <div className="capacity" style={{ marginBottom: 14 }}>
                <div className="stat">
                    <b>{fmtBytes(vol.on_disk_bytes)}</b>
                    <span>On disk</span>
                </div>
                <div className="meter">
                    <span style={{ width: `${pct}%` }} />
                </div>
                <div className="stat">
                    <b>{fmtBytes(vol.logical_bytes)}</b>
                    <span>Logical</span>
                </div>
                <div className="stat">
                    <b style={{ color: 'var(--green)' }}>{fmtBytes(saved)}</b>
                    <span>COW saved</span>
                </div>
            </div>

            <div className="settings-group">
                <div className="settings-head">
                    <span>Writable partitions</span>
                </div>
                <div className="card" style={{ boxShadow: 'var(--shadow)' }}>
                    {vol.writable_partitions.map((p) => (
                        <div key={p.name} className="device-row">
                            <div className="device-main">
                                <div className="device-label">{p.name}</div>
                            </div>
                            <span className="mono" style={{ fontSize: 12 }}>
                                {fmtBytes(p.size_bytes)}
                            </span>
                        </div>
                    ))}
                    {vol.writable_partitions.length === 0 && (
                        <div className="device-row">
                            <div className="device-main">
                                <div className="device-detail">
                                    No writable partitions detected in the instance directory.
                                </div>
                            </div>
                        </div>
                    )}
                </div>
            </div>

            <div className="settings-group">
                <div className="settings-head">
                    <span>Snapshots</span>
                    <span className="settings-gate">Phase 2 placeholder</span>
                </div>
                <div className="form-card">
                    <div className="snap-row" style={{ borderTop: 'none', paddingTop: 0 }}>
                        <span className="snap-label">
                            {vol.snapshots.length === 0
                                ? 'None yet'
                                : `${vol.snapshots.length} snapshot(s)`}
                        </span>
                        <button
                            className="btn"
                            onClick={trySnapshot}
                            title="Phase 2 — clonefile the writable partitions"
                        >
                            + Snapshot
                        </button>
                        <button className="btn" disabled>
                            Restore
                        </button>
                    </div>
                    {snapMsg && (
                        <p className="hint" style={{ color: 'var(--amber)' }}>
                            {snapMsg}
                        </p>
                    )}
                </div>
            </div>

            <p className="section-note mono" style={{ fontSize: 11 }}>
                {vol.instance_dir}
            </p>
        </div>
    )
}
