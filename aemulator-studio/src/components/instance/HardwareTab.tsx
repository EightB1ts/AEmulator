// The device inventory, restructured. The old global Devices page rendered 23
// flat rows grouped by honesty tier, and the 10 unusable "Planned" rows ate the
// most vertical space because each one drew an inert placeholder widget.
//
// Here the grouping axis is the SUBSYSTEM (DeviceEntry.group) and honesty is a
// per-item dot, so Graphics' real and aspirational halves sit together. All
// Planned items collapse behind one disclosure; their Phase-4 inject forms are
// preserved (DESIGN.md §4 wires them later) but gated behind a single toggle
// instead of being drawn 10 times unbidden.

import { useEffect, useState } from 'react'
import * as ipc from '../../lib/ipc'
import type { DeviceConfig, DeviceEntry, DeviceGroup, DeviceKind } from '../../lib/types'

const GROUP_TITLE: Record<DeviceGroup, string> = {
    Compute: 'Compute',
    Graphics: 'Graphics',
    Storage: 'Storage',
    Io: 'Input & I/O',
    Sensors: 'Sensors & radios',
    Security: 'Security',
}

// Render order (roughly boot order: compute → display → disk → I/O → the rest).
const GROUP_ORDER: DeviceGroup[] = [
    'Compute',
    'Graphics',
    'Storage',
    'Io',
    'Sensors',
    'Security',
]

// Inert placeholder controls for Planned devices — the shape of the eventual
// inject verbs (GPS fix, sensor values, battery level, …) that Phase 4 wires
// once the VMM grows the backends. Disabled by construction.
function PlaceholderControl({ kind }: { kind: DeviceKind }) {
    switch (kind) {
        case 'Gps':
            return (
                <div className="placeholder-ctl">
                    <input disabled placeholder="lat  37.7749" />
                    <input disabled placeholder="lng  -122.4194" />
                    <button className="btn" disabled>
                        Set fix
                    </button>
                </div>
            )
        case 'Battery':
            return (
                <div className="placeholder-ctl">
                    <input disabled type="range" min={0} max={100} defaultValue={80} />
                    <span className="mono">80%</span>
                    <label className="check inline-check">
                        <input type="checkbox" disabled /> <span>Charging</span>
                    </label>
                </div>
            )
        case 'Sensors':
            return (
                <div className="placeholder-ctl col">
                    {['accel x', 'accel y', 'accel z'].map((s) => (
                        <div className="slider-row" key={s}>
                            <span className="slider-label">{s}</span>
                            <input disabled type="range" min={-100} max={100} defaultValue={0} />
                        </div>
                    ))}
                </div>
            )
        case 'Audio':
            return (
                <div className="placeholder-ctl">
                    <button className="btn" disabled>
                        ▶ Play test tone
                    </button>
                    <input disabled type="range" min={0} max={100} defaultValue={60} />
                </div>
            )
        case 'Camera':
            return (
                <div className="placeholder-ctl">
                    <select disabled>
                        <option>Emulated scene</option>
                    </select>
                    <button className="btn" disabled>
                        Attach image source
                    </button>
                </div>
            )
        case 'Clipboard':
            return (
                <div className="placeholder-ctl">
                    <input disabled placeholder="push text to guest clipboard" />
                    <button className="btn" disabled>
                        Push
                    </button>
                </div>
            )
        case 'Storage':
            return (
                <div className="placeholder-ctl">
                    <button className="btn" disabled>
                        Insert SD image…
                    </button>
                </div>
            )
        case 'Vibration':
            return (
                <div className="placeholder-ctl">
                    <button className="btn" disabled>
                        Buzz 200ms
                    </button>
                </div>
            )
        case 'Bluetooth':
        case 'Nfc':
            return (
                <div className="placeholder-ctl">
                    <label className="check inline-check">
                        <input type="checkbox" disabled /> <span>Enable adapter</span>
                    </label>
                </div>
            )
        default:
            return null
    }
}

function Chip({
    d,
    showGroup,
    control,
    onGoToDisplay,
}: {
    d: DeviceEntry
    showGroup?: boolean
    control?: boolean
    onGoToDisplay?: () => void
}) {
    const cls = d.status.toLowerCase()
    return (
        <div className={`hw-chip ${cls}`}>
            <span className={`hw-dot ${cls}`} title={d.status} />
            <div className="hw-body">
                <div className="hw-label">
                    {d.label}
                    {showGroup && <span className="hw-group-tag">{GROUP_TITLE[d.group]}</span>}
                    {d.controllable && d.status === 'Emulated' && onGoToDisplay && (
                        // Fixes the old page's dead end: this used to be an inert
                        // "live control" pill while the actual controls lived on a
                        // different screen.
                        <button className="hw-live" onClick={onGoToDisplay}>
                            live control ↗
                        </button>
                    )}
                </div>
                <div className="hw-detail">{d.detail}</div>
                {control && <PlaceholderControl kind={d.kind} />}
            </div>
        </div>
    )
}

export function HardwareTab({
    id,
    onError,
    onGoToDisplay,
}: {
    id: string
    onError: (msg: string) => void
    onGoToDisplay: () => void
}) {
    const [config, setConfig] = useState<DeviceConfig | null>(null)
    const [openPlanned, setOpenPlanned] = useState(false)
    const [showInject, setShowInject] = useState(false)

    useEffect(() => {
        setConfig(null)
        ipc.deviceConfig(id).then(setConfig).catch((e) => onError(String(e)))
    }, [id])

    if (!config) return <div className="content" />

    const live = config.devices.filter((d) => d.status !== 'Planned')
    const planned = config.devices.filter((d) => d.status === 'Planned')
    const counts = {
        Emulated: config.devices.filter((d) => d.status === 'Emulated').length,
        Partial: config.devices.filter((d) => d.status === 'Partial').length,
        Planned: planned.length,
    }

    return (
        <div className="content">
            <p className="section-note">
                What this emulator actually provides, by subsystem.{' '}
                <b style={{ color: 'var(--green)' }}>{counts.Emulated} emulated</b> ·{' '}
                <b style={{ color: 'var(--amber)' }}>{counts.Partial} partial</b> ·{' '}
                <b>{counts.Planned} planned</b>. A filled dot is a working data path; a hollow one
                is metadata or a stub.
            </p>

            {GROUP_ORDER.map((g) => {
                const rows = live.filter((d) => d.group === g)
                if (rows.length === 0) return null
                return (
                    <div className="hw-section" key={g}>
                        <div className="hw-section-head">{GROUP_TITLE[g]}</div>
                        <div className="hw-grid">
                            {rows.map((d) => (
                                <Chip key={d.kind} d={d} onGoToDisplay={onGoToDisplay} />
                            ))}
                        </div>
                    </div>
                )
            })}

            {planned.length > 0 && (
                <div className="hw-planned">
                    <button
                        className="hw-disclosure"
                        onClick={() => setOpenPlanned((v) => !v)}
                        aria-expanded={openPlanned}
                    >
                        <span className="hw-caret">{openPlanned ? '▾' : '▸'}</span>
                        {planned.length} not yet emulated
                        <span className="hw-planned-list">
                            {planned.map((d) => d.label.split(/[—(]/)[0].trim()).join(' · ')}
                        </span>
                    </button>
                    {openPlanned && (
                        <>
                            <label className="check inline-check hw-inject-toggle">
                                <input
                                    type="checkbox"
                                    checked={showInject}
                                    onChange={(e) => setShowInject(e.target.checked)}
                                />
                                <span>
                                    Show the Phase-4 inject controls (inert — no backend behind
                                    them yet)
                                </span>
                            </label>
                            <div className="hw-grid">
                                {planned.map((d) => (
                                    <Chip key={d.kind} d={d} showGroup control={showInject} />
                                ))}
                            </div>
                        </>
                    )}
                </div>
            )}
        </div>
    )
}
