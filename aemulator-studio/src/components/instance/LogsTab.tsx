// Full-height console log. On the old Running screen this was wedged into the
// right rail under the control bar, which left ~200px for boot output — the one
// thing you actually read when a VM won't come up.
//
// Adds the DESIGN.md §3.3 filter: a substring match plus a stderr-only toggle.

import { useEffect, useMemo, useRef, useState } from 'react'
import type { VmLogEvent } from '../../lib/types'

export function LogsTab({ logs }: { logs: VmLogEvent[] }) {
    const [q, setQ] = useState('')
    const [errOnly, setErrOnly] = useState(false)
    const [pinned, setPinned] = useState(true)
    const bodyRef = useRef<HTMLDivElement | null>(null)

    const shown = useMemo(() => {
        const needle = q.trim().toLowerCase()
        return logs.filter((l) => {
            if (errOnly && l.stream !== 'stderr') return false
            if (needle && !l.line.toLowerCase().includes(needle)) return false
            return true
        })
    }, [logs, q, errOnly])

    // Follow the tail unless the user has scrolled up.
    useEffect(() => {
        if (pinned && bodyRef.current) {
            bodyRef.current.scrollTop = bodyRef.current.scrollHeight
        }
    }, [shown, pinned])

    function onScroll() {
        const el = bodyRef.current
        if (!el) return
        const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24
        setPinned(atBottom)
    }

    return (
        <div className="content logs-content">
            <div className="log-toolbar">
                <input
                    value={q}
                    onChange={(e) => setQ(e.target.value)}
                    placeholder="filter lines…"
                />
                <label className="check inline-check">
                    <input
                        type="checkbox"
                        checked={errOnly}
                        onChange={(e) => setErrOnly(e.target.checked)}
                    />
                    <span>stderr only</span>
                </label>
                <div className="spacer" />
                <span className="log-count">
                    {shown.length}
                    {shown.length !== logs.length && ` / ${logs.length}`}
                </span>
                {!pinned && (
                    <button
                        className="btn"
                        onClick={() => {
                            setPinned(true)
                            if (bodyRef.current)
                                bodyRef.current.scrollTop = bodyRef.current.scrollHeight
                        }}
                    >
                        ↓ Follow
                    </button>
                )}
            </div>

            <div className="log-pane tall">
                <div className="log-body mono" ref={bodyRef} onScroll={onScroll}>
                    {shown.map((l, i) => (
                        <div key={i} className={`log-line ${l.stream}`}>
                            {l.line}
                        </div>
                    ))}
                    {shown.length === 0 && (
                        <div className="log-line" style={{ opacity: 0.5 }}>
                            {logs.length === 0
                                ? '(no output yet — start the emulator)'
                                : '(no lines match the filter)'}
                        </div>
                    )}
                </div>
            </div>
        </div>
    )
}
