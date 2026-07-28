// The global storage surface, now scoped to what is genuinely cross-instance:
// the base-image catalog and a per-emulator roll-up. Each row links into that
// emulator's own Storage tab, which owns the COW meter, partition breakdown and
// snapshots — the detail used to be inlined here for every instance at once.

import { useEffect, useState } from 'react'
import * as ipc from '../lib/ipc'
import type { EmulatorProfile, ImageCatalogEntry, StorageVolume } from '../lib/types'
import { fmtBytes } from '../lib/format'

export function Storage({
    onOpenInstance,
}: {
    onOpenInstance: (id: string, tab: 'storage') => void
}) {
    const [vols, setVols] = useState<StorageVolume[]>([])
    const [images, setImages] = useState<ImageCatalogEntry[]>([])
    const [profiles, setProfiles] = useState<EmulatorProfile[]>([])
    const [err, setErr] = useState<string | null>(null)

    function refresh() {
        ipc.listStorage().then(setVols).catch((e) => setErr(String(e)))
        ipc.listBaseImages().then(setImages).catch(() => {})
        ipc.listEmulators().then(setProfiles).catch(() => {})
    }

    useEffect(() => {
        refresh()
        const t = setInterval(refresh, 4000)
        return () => clearInterval(t)
    }, [])

    const nameOf = (id: string) => profiles.find((p) => p.id === id)?.name ?? id
    const totalOnDisk = vols.reduce((s, v) => s + v.on_disk_bytes, 0)
    const totalLogical = vols.reduce((s, v) => s + v.logical_bytes, 0)

    return (
        <>
            <div className="toolbar" data-tauri-drag-region="deep">
                <h1>Images &amp; storage</h1>
            </div>
            <div className="content">
                {err && (
                    <p className="section-note" style={{ color: 'var(--red)' }}>
                        {err}
                    </p>
                )}

                <div className="settings-group">
                    <div className="settings-head">
                        <span>Base images</span>
                    </div>
                    <div className="card" style={{ boxShadow: 'var(--shadow)' }}>
                        {images.map((im) => (
                            <div key={im.key} className="device-row">
                                <div className="device-main">
                                    <div className="device-label">
                                        {im.key}{' '}
                                        <span
                                            className="mono"
                                            style={{ fontSize: 11, color: 'var(--text-faint)' }}
                                        >
                                            {im.path}
                                        </span>
                                    </div>
                                    <div className="device-detail">
                                        {im.present
                                            ? `${fmtBytes(im.logical_bytes)} logical`
                                            : 'not installed'}
                                    </div>
                                </div>
                                <span
                                    className={`status-badge ${im.present ? 'emulated' : 'planned'}`}
                                >
                                    {im.present ? 'Installed' : 'Missing'}
                                </span>
                            </div>
                        ))}
                        {images.length === 0 && (
                            <div className="device-row">
                                <div className="device-main">
                                    <div className="device-detail">
                                        No base images found under testdata/.
                                    </div>
                                </div>
                            </div>
                        )}
                    </div>
                </div>

                <div className="settings-group">
                    <div className="settings-head">
                        <span>Instance disks</span>
                        {vols.length > 0 && (
                            <span className="settings-gate">
                                {fmtBytes(totalOnDisk)} on disk · {fmtBytes(totalLogical)} logical
                            </span>
                        )}
                    </div>
                    <div className="card" style={{ boxShadow: 'var(--shadow)' }}>
                        {vols.map((v) => {
                            const saved = v.logical_bytes - v.on_disk_bytes
                            return (
                                <button
                                    key={v.emulator_id}
                                    className="device-row row-link"
                                    onClick={() => onOpenInstance(v.emulator_id, 'storage')}
                                >
                                    <div className="device-main">
                                        <div className="device-label">{nameOf(v.emulator_id)}</div>
                                        <div className="device-detail">
                                            {v.on_disk_bytes > 0
                                                ? `${fmtBytes(v.on_disk_bytes)} on disk · ${fmtBytes(saved)} saved by COW · ${v.snapshots.length} snapshot(s)`
                                                : 'Not provisioned — starts on first boot'}
                                        </div>
                                    </div>
                                    <span
                                        className={`status-badge ${v.on_disk_bytes > 0 ? 'emulated' : 'planned'}`}
                                    >
                                        {v.on_disk_bytes > 0 ? 'Provisioned' : 'Empty'}
                                    </span>
                                    <span className="row-chev">›</span>
                                </button>
                            )
                        })}
                        {vols.length === 0 && !err && (
                            <div className="device-row">
                                <div className="device-main">
                                    <div className="device-detail">
                                        No instance directories yet. Create an emulator to
                                        provision one.
                                    </div>
                                </div>
                            </div>
                        )}
                    </div>
                </div>
            </div>
        </>
    )
}
