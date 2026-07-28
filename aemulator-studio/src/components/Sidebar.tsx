// Two global surfaces. Device config used to be a third — but it's a property OF
// an emulator, so it moved to the per-instance hub's Hardware tab rather than
// living here behind a "which emulator?" dropdown.
export type NavKey = 'dashboard' | 'storage'

const ITEMS: { key: NavKey; label: string; icon: string }[] = [
    { key: 'dashboard', label: 'Emulators', icon: '▦' },
    { key: 'storage', label: 'Images', icon: '⛁' },
]

export function Sidebar({
    active,
    onNavigate,
}: {
    active: NavKey
    onNavigate: (k: NavKey) => void
}) {
    return (
        <aside className="sidebar">
            {/* Traffic-light inset strip — the sidebar's drag handle. */}
            <div className="sidebar-drag" data-tauri-drag-region />
            <div className="brand">
                <div className="brand-mark">Æ</div>
            </div>

            <div className="nav-group-label">Manage</div>
            {ITEMS.map((it) => (
                <button
                    key={it.key}
                    className={`nav-item${active === it.key ? ' active' : ''}`}
                    onClick={() => onNavigate(it.key)}
                >
                    <span className="ico">{it.icon}</span>
                    {it.label}
                </button>
            ))}
        </aside>
    )
}
