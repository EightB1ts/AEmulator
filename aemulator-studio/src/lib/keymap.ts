// Browser `KeyboardEvent.code` → evdev `KEY_*` code, for live keyboard capture on
// the Display tab.
//
// Why `code` and not `key`: both `KeyboardEvent.code` and evdev are POSITIONAL
// (physical key on a US QWERTY layout), so the mapping is 1:1 and layout-independent
// — the guest applies its own keymap to the position, exactly as real hardware does.
// Mapping `event.key` instead would double-apply the host layout and turn a French
// AZERTY 'a' into a guest 'q'.
//
// Only codes < 256 are deliverable — the virtio-input keyboard advertises EV_KEY
// 0..255 (see `crates/aemulator-hvf/src/virtio/input.rs`), so Android's high-range
// keys (KEY_APPSELECT 580 = Recents) are NOT reachable this way; that's what the
// Menu/Back/Home buttons in the control bar are for.

export const CODE_TO_EVDEV: Record<string, number> = {
    // Letters (positional).
    KeyA: 30, KeyB: 48, KeyC: 46, KeyD: 32, KeyE: 18, KeyF: 33, KeyG: 34, KeyH: 35,
    KeyI: 23, KeyJ: 36, KeyK: 37, KeyL: 38, KeyM: 50, KeyN: 49, KeyO: 24, KeyP: 25,
    KeyQ: 16, KeyR: 19, KeyS: 31, KeyT: 20, KeyU: 22, KeyV: 47, KeyW: 17, KeyX: 45,
    KeyY: 21, KeyZ: 44,

    // Digit row.
    Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6,
    Digit6: 7, Digit7: 8, Digit8: 9, Digit9: 10, Digit0: 11,

    // Punctuation.
    Minus: 12, Equal: 13, BracketLeft: 26, BracketRight: 27, Backslash: 43,
    Semicolon: 39, Quote: 40, Backquote: 41, Comma: 51, Period: 52, Slash: 53,

    // Editing + whitespace.
    Space: 57, Enter: 28, Tab: 15, Backspace: 14, Delete: 111, Escape: 1,
    CapsLock: 58,

    // Modifiers. The Mac Command key arrives as Meta{Left,Right} → KEY_LEFTMETA /
    // KEY_RIGHTMETA (125/126), which Android reads as its own META modifier — not a
    // second Ctrl. Option is Alt.
    ShiftLeft: 42, ShiftRight: 54,
    ControlLeft: 29, ControlRight: 97,
    AltLeft: 56, AltRight: 100,
    MetaLeft: 125, MetaRight: 126,

    // Navigation.
    ArrowUp: 103, ArrowDown: 108, ArrowLeft: 105, ArrowRight: 106,
    Home: 102, End: 107, PageUp: 104, PageDown: 109, Insert: 110,

    // Function row (fn-less presses; on a Mac keyboard these need fn held unless
    // "Use F1, F2 as standard function keys" is on).
    F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64,
    F7: 65, F8: 66, F9: 67, F10: 68, F11: 87, F12: 88,

    // Numpad.
    NumpadDivide: 98, NumpadMultiply: 55, NumpadSubtract: 74, NumpadAdd: 78,
    NumpadEnter: 96, NumpadDecimal: 83,
    Numpad0: 82, Numpad1: 79, Numpad2: 80, Numpad3: 81, Numpad4: 75,
    Numpad5: 76, Numpad6: 77, Numpad7: 71, Numpad8: 72, Numpad9: 73,
}

/// The evdev code for a physical key, or null when we have no key for it (dead
/// keys, media keys, `fn` — which macOS never reports as a keydown at all).
export function evdevForCode(code: string): number | null {
    return CODE_TO_EVDEV[code] ?? null
}

/// Keys that must NOT be swallowed even while capture is armed, because taking them
/// would strand the user inside the captured surface. Cmd+Q is the OS quit path;
/// Cmd+W/M/H are window management. We let macOS have them.
const HOST_RESERVED_WITH_META = new Set(['KeyQ', 'KeyW', 'KeyM', 'KeyH', 'Tab', 'Space', 'Backquote'])

/// True when this event belongs to the host, not the guest: a Command chord the OS
/// or the app menu owns. Callers should leave these alone (no preventDefault, no
/// forward) so quit/close/Spotlight/app-switch keep working.
///
/// Note the honest limitation: macOS consumes Cmd+Tab, Cmd+Space and Cmd+` before
/// the WebView ever sees a keydown, and Tauri's default app menu claims Cmd+Q/W/M/H
/// plus the Edit-menu chords (Cmd+A/C/V/X/Z) as key equivalents. Those combos cannot
/// be forwarded to the guest from a normal window, capture armed or not — the Android
/// side has to be driven with its own on-screen affordances or the control bar.
export function isHostReservedChord(ev: KeyboardEvent): boolean {
    if (!ev.metaKey) return false
    // A bare Command press is forwardable (it's the modifier itself); a chord is not.
    if (ev.code === 'MetaLeft' || ev.code === 'MetaRight') return false
    return HOST_RESERVED_WITH_META.has(ev.code)
}
