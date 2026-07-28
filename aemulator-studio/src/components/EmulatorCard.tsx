import type { EmulatorProfile, VmState } from '../lib/types'
import { fmtMb } from '../lib/format'

const STATE_LABEL: Record<VmState, string> = {
    Provisioning: 'Provisioning',
    Booting: 'Booting',
    Ready: 'Ready',
    Stopping: 'Stopping',
    Stopped: 'Stopped',
    Crashed: 'Crashed',
}

function StatePill({ state }: { state: VmState }) {
    return (
        <span className={`pill ${state.toLowerCase()}`}>
            <span className="dot" />
            {STATE_LABEL[state]}
        </span>
    )
}

const RENDER_LABEL: Record<string, string> = {
    Gpu3d: 'Metal GPU',
    SwRender: 'SwRender',
    Headless: 'Headless',
}

export function EmulatorCard({
    profile,
    state,
    busy,
    onStart,
    onStop,
    onOpen,
    onDelete,
}: {
    profile: EmulatorProfile
    state: VmState
    busy: boolean
    onStart: () => void
    onStop: () => void
    onOpen: () => void
    onDelete: () => void
}) {
    const running = state === 'Ready' || state === 'Booting' || state === 'Provisioning'
    const thumb =
        state === 'Ready'
            ? 'live · open to view'
            : state === 'Booting'
              ? 'booting…'
              : state === 'Crashed'
                ? 'crashed — check logs'
                : `${profile.base_image} · idle`
    return (
        <div className="card">
            {/* Always enabled: the card now opens the per-instance hub (hardware,
                settings, storage, logs), which is useful stopped as well as live. */}
            <button
                className="thumb thumb-btn"
                onClick={onOpen}
                title={running ? 'Open live view' : 'Open this emulator'}
            >
                {thumb}
            </button>
            <div className="body">
                <div className="title-row">
                    <span className="title">{profile.name}</span>
                    <StatePill state={state} />
                </div>
                <div className="meta">
                    <span>{fmtMb(profile.mem_mb)} RAM</span>
                    <span>{profile.cpus} vCPU</span>
                    <span>{RENDER_LABEL[profile.render_mode] ?? profile.render_mode}</span>
                </div>
                <div className="meta">
                    <span className="mono">{profile.device_identity.model}</span>
                    <span>{profile.device_identity.soc}</span>
                </div>
                <div className="foot">
                    {running ? (
                        <button className="btn danger" disabled={busy} onClick={onStop}>
                            Stop
                        </button>
                    ) : (
                        <button className="btn primary" disabled={busy} onClick={onStart}>
                            Start
                        </button>
                    )}
                    <button className="btn" onClick={onOpen}>
                        Open
                    </button>
                    <div style={{ flex: 1 }} />
                    <button
                        className="btn icon-btn"
                        disabled={busy || running}
                        onClick={onDelete}
                        title={running ? 'Stop before deleting' : 'Delete emulator'}
                    >
                        🗑
                    </button>
                </div>
            </div>
        </div>
    )
}
