import { useEffect, useMemo, useState } from 'react'
import * as ipc from '../lib/ipc'
import type {
    CreateEmulatorInput,
    ImageCatalogEntry,
    RenderMode,
    SelinuxMode,
} from '../lib/types'
import { fmtBytes } from '../lib/format'

const RENDER_OPTIONS: { mode: RenderMode; title: string; blurb: string }[] = [
    {
        mode: 'SwRender',
        title: 'SwRender (recommended)',
        blurb: 'Guest-CPU SwiftShader/ANGLE into the scanout (SWRENDER=1). No native window — the most stable headless boot.',
    },
    {
        mode: 'Gpu3d',
        title: 'Metal GPU',
        blurb: 'gfxstream → KosmicKrisp (Vulkan-on-Metal) → Metal. Presents through a Vulkan swapchain on the window layer — no CPU readback. Opens a native window. Needs macOS 26+ on Apple Silicon and the binary built --features gfxstream.',
    },
    {
        mode: 'Headless',
        title: 'Headless',
        blurb: 'No render pipeline flags at all. Boots + is controllable over the socket; the framebuffer stays blank.',
    },
]

const STEPS = ['Basics', 'Render & security', 'Identity', 'Review']

export function CreateEmulator({
    onCancel,
    onCreated,
}: {
    onCancel: () => void
    onCreated: (id: string) => void
}) {
    const [step, setStep] = useState(0)
    const [images, setImages] = useState<ImageCatalogEntry[]>([])
    const [busy, setBusy] = useState(false)
    const [err, setErr] = useState<string | null>(null)

    // Form state
    const [name, setName] = useState('New Cuttlefish')
    const [baseImage, setBaseImage] = useState('cf64')
    const [memMb, setMemMb] = useState(4096)
    const [cpus, setCpus] = useState(2)
    const [renderMode, setRenderMode] = useState<RenderMode>('SwRender')
    const [selinux, setSelinux] = useState<SelinuxMode>('Permissive')
    const [soc, setSoc] = useState('Snapdragon 8 Gen 2 / SM8550')
    const [manufacturer, setManufacturer] = useState('Xiaomi')
    const [model, setModel] = useState('fuxi')
    const [sensors, setSensors] = useState('accelerometer, magnetometer')
    const [telephony, setTelephony] = useState(true)
    const [imei, setImei] = useState('350000000000001')
    const [operator, setOperator] = useState('Verizon')
    const [mccMnc, setMccMnc] = useState('311480')
    const [extraCmdline, setExtraCmdline] = useState('')

    useEffect(() => {
        ipc.listBaseImages()
            .then((imgs) => {
                setImages(imgs)
                const present = imgs.find((i) => i.present)
                if (present) setBaseImage(present.key)
            })
            .catch((e) => setErr(String(e)))
    }, [])

    const input: CreateEmulatorInput = useMemo(
        () => ({
            name: name.trim() || 'New Cuttlefish',
            base_image: baseImage,
            mem_mb: memMb,
            cpus,
            render_mode: renderMode,
            selinux,
            extra_cmdline: extraCmdline.trim(),
            device_identity: {
                soc,
                manufacturer,
                model,
                advertised_sensors: sensors
                    .split(',')
                    .map((s) => s.trim())
                    .filter(Boolean),
                telephony: telephony
                    ? { imei, operator, mcc_mnc: mccMnc }
                    : null,
            },
        }),
        [name, baseImage, memMb, cpus, renderMode, selinux, extraCmdline, soc, manufacturer, model, sensors, telephony, imei, operator, mccMnc],
    )

    async function create() {
        setBusy(true)
        setErr(null)
        try {
            const profile = await ipc.createEmulator(input)
            onCreated(profile.id)
        } catch (e) {
            setErr(String(e))
            setBusy(false)
        }
    }

    return (
        <>
            <div className="toolbar" data-tauri-drag-region="deep">
                <button className="btn" onClick={onCancel}>
                    ← Cancel
                </button>
                <h1 style={{ marginLeft: 4 }}>New Emulator</h1>
                <div className="spacer" />
                <div className="stepper">
                    {STEPS.map((s, i) => (
                        <span
                            key={s}
                            className={`step-dot${i === step ? ' active' : ''}${i < step ? ' done' : ''}`}
                        >
                            {s}
                        </span>
                    ))}
                </div>
            </div>

            <div className="content wizard">
                {err && (
                    <p className="section-note" style={{ color: 'var(--red)' }}>
                        {err}
                    </p>
                )}

                {step === 0 && (
                    <div className="form-card">
                        <label className="field">
                            <span>Name</span>
                            <input value={name} onChange={(e) => setName(e.target.value)} />
                        </label>
                        <label className="field">
                            <span>Base image</span>
                            <select value={baseImage} onChange={(e) => setBaseImage(e.target.value)}>
                                {images.map((i) => (
                                    <option key={i.key} value={i.key} disabled={!i.present}>
                                        {i.key}
                                        {i.present ? ` · ${fmtBytes(i.logical_bytes)}` : ' · not installed'}
                                    </option>
                                ))}
                                {images.length === 0 && <option value="cf64">cf64</option>}
                            </select>
                        </label>
                        <div className="field-row">
                            <label className="field">
                                <span>Memory</span>
                                <select value={memMb} onChange={(e) => setMemMb(+e.target.value)}>
                                    {[4096, 6144, 8192, 12288].map((m) => (
                                        <option key={m} value={m}>
                                            {m / 1024} GB
                                        </option>
                                    ))}
                                </select>
                            </label>
                            <label className="field">
                                <span>vCPUs</span>
                                <select value={cpus} onChange={(e) => setCpus(+e.target.value)}>
                                    {[2, 4, 6, 8].map((c) => (
                                        <option key={c} value={c}>
                                            {c}
                                        </option>
                                    ))}
                                </select>
                            </label>
                        </div>
                        <p className="hint">
                            Cuttlefish needs ≥ 4 GB to clear init reliably. Each running VM
                            is budgeted against host RAM.
                        </p>
                    </div>
                )}

                {step === 1 && (
                    <div className="form-card">
                        <div className="field-label">Render pipeline</div>
                        {RENDER_OPTIONS.map((o) => (
                            <label key={o.mode} className={`radio-card${renderMode === o.mode ? ' sel' : ''}`}>
                                <input
                                    type="radio"
                                    name="render"
                                    checked={renderMode === o.mode}
                                    onChange={() => setRenderMode(o.mode)}
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
                                    className={`seg-btn${selinux === s ? ' active' : ''}`}
                                    onClick={() => setSelinux(s)}
                                >
                                    {s}
                                </button>
                            ))}
                        </div>
                        <p className="hint">
                            Permissive is the reliable default for bring-up; enforcing exercises
                            the real policy but can wedge on missing labels.
                        </p>
                    </div>
                )}

                {step === 2 && (
                    <div className="form-card">
                        <p className="hint" style={{ marginTop: 0 }}>
                            Identity is injected into the guest's device tree (real-SoC spoof) so
                            Android reports this hardware instead of the synthetic default.
                        </p>
                        <div className="field-row">
                            <label className="field">
                                <span>SoC</span>
                                <input value={soc} onChange={(e) => setSoc(e.target.value)} />
                            </label>
                        </div>
                        <div className="field-row">
                            <label className="field">
                                <span>Manufacturer</span>
                                <input value={manufacturer} onChange={(e) => setManufacturer(e.target.value)} />
                            </label>
                            <label className="field">
                                <span>Model</span>
                                <input value={model} onChange={(e) => setModel(e.target.value)} />
                            </label>
                        </div>
                        <label className="field">
                            <span>Advertised sensors (comma-separated)</span>
                            <input value={sensors} onChange={(e) => setSensors(e.target.value)} />
                        </label>
                        <label className="check">
                            <input
                                type="checkbox"
                                checked={telephony}
                                onChange={(e) => setTelephony(e.target.checked)}
                            />
                            <span>Advertise telephony (IMEI / operator metadata — no live radio)</span>
                        </label>
                        {telephony && (
                            <div className="field-row">
                                <label className="field">
                                    <span>IMEI</span>
                                    <input value={imei} onChange={(e) => setImei(e.target.value)} />
                                </label>
                                <label className="field">
                                    <span>Operator</span>
                                    <input value={operator} onChange={(e) => setOperator(e.target.value)} />
                                </label>
                                <label className="field">
                                    <span>MCC/MNC</span>
                                    <input value={mccMnc} onChange={(e) => setMccMnc(e.target.value)} />
                                </label>
                            </div>
                        )}
                        <label className="field">
                            <span>Extra kernel cmdline (advanced, optional)</span>
                            <input
                                value={extraCmdline}
                                onChange={(e) => setExtraCmdline(e.target.value)}
                                placeholder="appended to the render bootconfig"
                            />
                        </label>
                    </div>
                )}

                {step === 3 && (
                    <div className="form-card">
                        <div className="field-label">Review</div>
                        <dl className="review">
                            <dt>Name</dt>
                            <dd>{input.name}</dd>
                            <dt>Base image</dt>
                            <dd>{input.base_image}</dd>
                            <dt>Resources</dt>
                            <dd>
                                {input.mem_mb / 1024} GB · {input.cpus} vCPU
                            </dd>
                            <dt>Render</dt>
                            <dd>{input.render_mode}</dd>
                            <dt>SELinux</dt>
                            <dd>{input.selinux}</dd>
                            <dt>Identity</dt>
                            <dd>
                                {input.device_identity.manufacturer} {input.device_identity.model} ·{' '}
                                {input.device_identity.soc}
                            </dd>
                            <dt>Sensors</dt>
                            <dd>
                                {input.device_identity.advertised_sensors.join(', ') || '—'}{' '}
                                <span className="tag-partial">declared only</span>
                            </dd>
                            <dt>Telephony</dt>
                            <dd>
                                {input.device_identity.telephony
                                    ? `${input.device_identity.telephony.operator} (${input.device_identity.telephony.imei})`
                                    : 'off'}
                                {input.device_identity.telephony && (
                                    <span className="tag-partial">metadata only</span>
                                )}
                            </dd>
                        </dl>
                        <p className="hint">
                            On first start a private copy-on-write disk image is provisioned for
                            this emulator (APFS clonefile of the base image) — instant, and it
                            keeps each VM's storage isolated.
                        </p>
                    </div>
                )}

                <div className="wizard-foot">
                    {step > 0 ? (
                        <button className="btn" onClick={() => setStep(step - 1)} disabled={busy}>
                            Back
                        </button>
                    ) : (
                        <button className="btn" onClick={onCancel} disabled={busy}>
                            Cancel
                        </button>
                    )}
                    <div className="spacer" />
                    {step < STEPS.length - 1 ? (
                        <button className="btn primary" onClick={() => setStep(step + 1)}>
                            Continue
                        </button>
                    ) : (
                        <button className="btn primary" onClick={create} disabled={busy}>
                            {busy ? 'Creating…' : 'Create emulator'}
                        </button>
                    )}
                </div>
            </div>
        </>
    )
}
