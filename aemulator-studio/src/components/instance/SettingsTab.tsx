// The editor the app never had — profile fields were write-once in the Create
// wizard with no edit path (there was no update_emulator command at all).
//
// Gating mirrors manager::update_emulator, which is the real boundary. The tiers
// come from what spawn::compose_launch actually consumes:
//   Always      name, device_identity        — neither reaches the launch
//   Stopped     mem/cpus/render/selinux/cmdline — composed into argv+env at start
//   Stopped +   base_image                   — provision_cf_dir reuses an existing
//   confirm                                    cf-dir, so a changed base without
//                                              erasing the disk would silently
//                                              keep booting the OLD image
// We reject rather than defer: a saved value that doesn't match the running VM
// is a lie the UI can't walk back.

import { useEffect, useMemo, useState } from 'react'
import * as ipc from '../../lib/ipc'
import type {
    EmulatorProfile,
    ImageCatalogEntry,
    RenderMode,
    SelinuxMode,
    UpdateEmulatorInput,
} from '../../lib/types'
import { fmtBytes } from '../../lib/format'

const RENDER_OPTIONS: { mode: RenderMode; title: string; blurb: string }[] = [
    {
        mode: 'SwRender',
        title: 'SwRender',
        blurb: 'Guest-CPU SwiftShader/ANGLE into the scanout (SWRENDER=1). No native window — the most stable headless boot.',
    },
    {
        mode: 'Gpu3d',
        title: 'Metal GPU',
        blurb: 'gfxstream → KosmicKrisp (Vulkan-on-Metal) → Metal. Presents through a Vulkan swapchain on the window layer — no CPU readback. Opens a native window. Needs macOS 26+ on Apple Silicon.',
    },
    {
        mode: 'Headless',
        title: 'Headless',
        blurb: 'No render pipeline flags at all. Boots + is controllable over the socket; the framebuffer stays blank.',
    },
]

type Draft = {
    name: string
    base_image: string
    mem_mb: number
    cpus: number
    render_mode: RenderMode
    selinux: SelinuxMode
    extra_cmdline: string
    soc: string
    manufacturer: string
    model: string
    sensors: string
    telephony: boolean
    imei: string
    operator: string
    mcc_mnc: string
}

function toDraft(p: EmulatorProfile): Draft {
    return {
        name: p.name,
        base_image: p.base_image,
        mem_mb: p.mem_mb,
        cpus: p.cpus,
        render_mode: p.render_mode,
        selinux: p.selinux,
        extra_cmdline: p.extra_cmdline,
        soc: p.device_identity.soc,
        manufacturer: p.device_identity.manufacturer,
        model: p.device_identity.model,
        sensors: p.device_identity.advertised_sensors.join(', '),
        telephony: p.device_identity.telephony != null,
        imei: p.device_identity.telephony?.imei ?? '350000000000001',
        operator: p.device_identity.telephony?.operator ?? 'Verizon',
        mcc_mnc: p.device_identity.telephony?.mcc_mnc ?? '311480',
    }
}

function identityOf(d: Draft) {
    return {
        soc: d.soc,
        manufacturer: d.manufacturer,
        model: d.model,
        advertised_sensors: d.sensors
            .split(',')
            .map((s) => s.trim())
            .filter(Boolean),
        telephony: d.telephony
            ? { imei: d.imei, operator: d.operator, mcc_mnc: d.mcc_mnc }
            : null,
    }
}

export function SettingsTab({
    profile,
    running,
    onSaved,
    onDeleted,
    onError,
}: {
    profile: EmulatorProfile
    running: boolean
    onSaved: (p: EmulatorProfile) => void
    onDeleted: () => void
    onError: (msg: string) => void
}) {
    const [draft, setDraft] = useState<Draft>(() => toDraft(profile))
    const [images, setImages] = useState<ImageCatalogEntry[]>([])
    const [busy, setBusy] = useState(false)

    // Re-seed when the profile identity changes underneath us (e.g. saved
    // elsewhere), but never clobber an in-progress edit of the same profile.
    useEffect(() => {
        setDraft(toDraft(profile))
    }, [profile.id])

    useEffect(() => {
        ipc.listBaseImages().then(setImages).catch(() => {})
    }, [])

    // Only send what changed — so the stopped-only gate trips on a real edit and
    // not on saving a name change while the VM happens to be up.
    const patch = useMemo<UpdateEmulatorInput>(() => {
        const p: UpdateEmulatorInput = {}
        if (draft.name.trim() !== profile.name) p.name = draft.name.trim()
        if (draft.base_image !== profile.base_image) p.base_image = draft.base_image
        if (draft.mem_mb !== profile.mem_mb) p.mem_mb = draft.mem_mb
        if (draft.cpus !== profile.cpus) p.cpus = draft.cpus
        if (draft.render_mode !== profile.render_mode) p.render_mode = draft.render_mode
        if (draft.selinux !== profile.selinux) p.selinux = draft.selinux
        if (draft.extra_cmdline !== profile.extra_cmdline) p.extra_cmdline = draft.extra_cmdline
        const nextId = identityOf(draft)
        if (JSON.stringify(nextId) !== JSON.stringify(profile.device_identity)) {
            p.device_identity = nextId
        }
        return p
    }, [draft, profile])

    const dirty = Object.keys(patch).length > 0
    const baseChanged = patch.base_image != null
    const locked = running // launch-affecting fields
    const set = <K extends keyof Draft>(k: K, v: Draft[K]) =>
        setDraft((d) => ({ ...d, [k]: v }))

    async function save() {
        if (baseChanged) {
            const ok = confirm(
                `Switch the base image to "${draft.base_image}"?\n\n` +
                    `This ERASES this emulator's provisioned disk (and everything the VM has ` +
                    `written) so the next start re-clones from the new base. There is no undo.`,
            )
            if (!ok) return
        }
        setBusy(true)
        try {
            const updated = await ipc.updateEmulator(profile.id, {
                ...patch,
                ...(baseChanged ? { reprovision: true } : {}),
            })
            onSaved(updated)
        } catch (e) {
            onError(String(e))
        } finally {
            setBusy(false)
        }
    }

    async function remove() {
        if (
            !confirm(
                `Delete "${profile.name}"? Its provisioned disk image is removed too.`,
            )
        )
            return
        setBusy(true)
        try {
            await ipc.deleteEmulator(profile.id)
            onDeleted()
        } catch (e) {
            onError(String(e))
            setBusy(false)
        }
    }

    return (
        <div className="content">
            {locked && (
                <div className="banner warn">
                    <b>The VM is running.</b> Name and identity still save, but memory, vCPUs,
                    render pipeline, SELinux, kernel cmdline and base image are composed into the
                    launch command at start — stop the VM to change them.
                </div>
            )}

            <div className="settings-group">
                <div className="settings-head">
                    <span>Profile</span>
                    <span className="settings-gate ok">applies immediately</span>
                </div>
                <div className="form-card">
                    <label className="field">
                        <span>Name</span>
                        <input value={draft.name} onChange={(e) => set('name', e.target.value)} />
                    </label>
                </div>
            </div>

            <div className="settings-group">
                <div className="settings-head">
                    <span>Runtime</span>
                    <span className={`settings-gate ${locked ? 'locked' : ''}`}>
                        {locked ? 'locked — VM running' : 'applies on next start'}
                    </span>
                </div>
                <div className={`form-card${locked ? ' is-locked' : ''}`}>
                    <div className="field-row">
                        <label className="field">
                            <span>Memory</span>
                            <select
                                value={draft.mem_mb}
                                disabled={locked}
                                onChange={(e) => set('mem_mb', +e.target.value)}
                            >
                                {[4096, 6144, 8192, 12288].map((m) => (
                                    <option key={m} value={m}>
                                        {m / 1024} GB
                                    </option>
                                ))}
                            </select>
                        </label>
                        <label className="field">
                            <span>vCPUs</span>
                            <select
                                value={draft.cpus}
                                disabled={locked}
                                onChange={(e) => set('cpus', +e.target.value)}
                            >
                                {[2, 4, 6, 8].map((c) => (
                                    <option key={c} value={c}>
                                        {c}
                                    </option>
                                ))}
                            </select>
                        </label>
                    </div>

                    <div className="field-label" style={{ marginTop: 12 }}>
                        Render pipeline
                    </div>
                    {RENDER_OPTIONS.map((o) => (
                        <label
                            key={o.mode}
                            className={`radio-card${draft.render_mode === o.mode ? ' sel' : ''}`}
                        >
                            <input
                                type="radio"
                                name="render"
                                disabled={locked}
                                checked={draft.render_mode === o.mode}
                                onChange={() => set('render_mode', o.mode)}
                            />
                            <div>
                                <div className="radio-title">{o.title}</div>
                                <div className="radio-blurb">{o.blurb}</div>
                            </div>
                        </label>
                    ))}

                    <div className="field-label" style={{ marginTop: 14 }}>
                        SELinux
                    </div>
                    <div className="seg">
                        {(['Permissive', 'Enforcing'] as SelinuxMode[]).map((s) => (
                            <button
                                key={s}
                                className={`seg-btn${draft.selinux === s ? ' active' : ''}`}
                                disabled={locked}
                                onClick={() => set('selinux', s)}
                            >
                                {s}
                            </button>
                        ))}
                    </div>

                    <label className="field" style={{ marginTop: 12 }}>
                        <span>Extra kernel cmdline (advanced)</span>
                        <input
                            value={draft.extra_cmdline}
                            disabled={locked}
                            onChange={(e) => set('extra_cmdline', e.target.value)}
                            placeholder="appended to the render bootconfig"
                        />
                    </label>
                </div>
            </div>

            <div className="settings-group">
                <div className="settings-head">
                    <span>Device identity</span>
                    <span className="settings-gate ok">applies immediately</span>
                </div>
                <div className="form-card">
                    <p className="hint" style={{ marginTop: 0 }}>
                        Stored on the profile and shown in this app + the Hardware inventory. Not
                        passed to the VM launch today — <code>compose_launch</code> takes no
                        identity arguments, so editing these can't affect a boot either way.
                    </p>
                    <label className="field">
                        <span>SoC</span>
                        <input value={draft.soc} onChange={(e) => set('soc', e.target.value)} />
                    </label>
                    <div className="field-row">
                        <label className="field">
                            <span>Manufacturer</span>
                            <input
                                value={draft.manufacturer}
                                onChange={(e) => set('manufacturer', e.target.value)}
                            />
                        </label>
                        <label className="field">
                            <span>Model</span>
                            <input
                                value={draft.model}
                                onChange={(e) => set('model', e.target.value)}
                            />
                        </label>
                    </div>
                    <label className="field">
                        <span>Advertised sensors (comma-separated)</span>
                        <input
                            value={draft.sensors}
                            onChange={(e) => set('sensors', e.target.value)}
                        />
                    </label>
                    <label className="check">
                        <input
                            type="checkbox"
                            checked={draft.telephony}
                            onChange={(e) => set('telephony', e.target.checked)}
                        />
                        <span>Advertise telephony (IMEI / operator metadata — no live radio)</span>
                    </label>
                    {draft.telephony && (
                        <div className="field-row">
                            <label className="field">
                                <span>IMEI</span>
                                <input
                                    value={draft.imei}
                                    onChange={(e) => set('imei', e.target.value)}
                                />
                            </label>
                            <label className="field">
                                <span>Operator</span>
                                <input
                                    value={draft.operator}
                                    onChange={(e) => set('operator', e.target.value)}
                                />
                            </label>
                            <label className="field">
                                <span>MCC/MNC</span>
                                <input
                                    value={draft.mcc_mnc}
                                    onChange={(e) => set('mcc_mnc', e.target.value)}
                                />
                            </label>
                        </div>
                    )}
                </div>
            </div>

            <div className="settings-group">
                <div className="settings-head">
                    <span>Base image</span>
                    <span className={`settings-gate ${locked ? 'locked' : 'danger'}`}>
                        {locked ? 'locked — VM running' : 'erases the disk'}
                    </span>
                </div>
                <div className={`form-card${locked ? ' is-locked' : ''}`}>
                    <label className="field">
                        <span>Image</span>
                        <select
                            value={draft.base_image}
                            disabled={locked}
                            onChange={(e) => set('base_image', e.target.value)}
                        >
                            {images.map((i) => (
                                <option key={i.key} value={i.key} disabled={!i.present}>
                                    {i.key}
                                    {i.present
                                        ? ` · ${fmtBytes(i.logical_bytes)}`
                                        : ' · not installed'}
                                </option>
                            ))}
                            {images.length === 0 && (
                                <option value={draft.base_image}>{draft.base_image}</option>
                            )}
                        </select>
                    </label>
                    <p className="hint">
                        The instance disk is a copy-on-write clone made at first start and reused
                        afterwards, so switching the base only takes effect if that clone is
                        discarded. Saving a change here does exactly that — this VM's userdata is
                        erased and the next start re-clones.
                    </p>
                </div>
            </div>

            <div className="settings-group">
                <div className="settings-head">
                    <span>Danger zone</span>
                </div>
                <div className="form-card">
                    <div className="inline">
                        <button className="btn danger" disabled={busy || running} onClick={remove}>
                            Delete emulator
                        </button>
                        <span className="hint" style={{ margin: 0 }}>
                            {running
                                ? 'Stop the VM first.'
                                : 'Removes the profile and its provisioned disk image.'}
                        </span>
                    </div>
                </div>
            </div>

            <div className="settings-foot">
                <span className="hint" style={{ margin: 0 }}>
                    {dirty
                        ? `${Object.keys(patch).length} unsaved change${Object.keys(patch).length === 1 ? '' : 's'}`
                        : 'No changes'}
                </span>
                <div className="spacer" />
                <button
                    className="btn"
                    disabled={!dirty || busy}
                    onClick={() => setDraft(toDraft(profile))}
                >
                    Revert
                </button>
                <button className="btn primary" disabled={!dirty || busy} onClick={save}>
                    {busy ? 'Saving…' : 'Save changes'}
                </button>
            </div>
        </div>
    )
}
