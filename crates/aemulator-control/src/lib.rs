//! # aemulator-control
//!
//! The **LLM control-plane contract** — the versioned, transport-agnostic API a client (Claude, or
//! later a thin MCP / computer-use adapter) drives the device through. There is intentionally **no
//! gRPC here**: this crate defines the Rust types + the [`DeviceController`] trait; a network shell
//! is a later, thin adapter over exactly this.
//!
//! ## This crate is the single backend
//!
//! Everything the agent can perceive or do goes through the one [`DeviceController`] trait. The
//! future surfaces are **thin shells that wrap this exact trait — no device logic lives in them**:
//!
//! - a **Rust MCP server** exposing `observe` / `act` / `wait_stable` as MCP tools; and
//! - an **Anthropic computer-use adapter** (tool version `computer_20251124`) that maps the
//!   `screenshot` / `left_click` / `type` / `key` computer-use actions onto [`Action`], translating
//!   the model's screenshot coordinates back into device pixels.
//!
//! Because both are pure translation layers, the design invariants below (index-first tapping, the
//! generation guard, post-action observations) are enforced **once, here**, and every client
//! inherits them for free.
//!
//! ## Design (from the plan)
//!
//! - **Index-first with a generation counter.** Every [`Observation`] carries a `generation`; the
//!   [`UiTree`] carries the same value. An [`Action::Tap`] names a node by `index` *and* the
//!   `generation` it was computed against. If the tree has changed since (generation advanced), the
//!   tap **fails loud** with [`ControlError::StaleGeneration`] rather than hitting the wrong element.
//!   Raw coordinates ([`Action::TapXy`]) are the explicit escape hatch. The generation is
//!   **monotonically increasing** — never rewound, even by [`Action::Restore`] — so a stale index
//!   can never accidentally re-validate against a later identical tree.
//! - **Every mutating call returns its post-action [`Observation`]** — the client never has to
//!   guess when to re-observe.
//!
//! ## Anti-detection rationale (why the seams are where they are)
//!
//! The control surface is a load-bearing anti-detection decision (see the architecture plan). Two
//! constraints shape this contract even though the [`MockController`] cannot enforce them offline:
//!
//! - The structured UI tree is meant to come from a **platform-privileged, non-enumerable** guest
//!   component — never an app-registered `AccessibilityService`, which anti-fraud SDKs enumerate via
//!   `getEnabledAccessibilityServiceList`.
//! - Real drivers inject at the **virtio-input** layer with genuine `InputDevice` provenance and
//!   **human-kinematic timing/jitter**, because mechanically-perfect input is itself a tell —
//!   Tencent ACE scores inter-touch-interval regularity (`ano_touch_period`) and its variance
//!   (`ano_touch_reg`), and flags exact-pixel taps as auto-clicker behaviour. Hence [`Action::Tap`]
//!   names a *node* (whose center the driver taps with jitter) rather than raw pixels.
//!
//! [`MockController`] is a real, fully-tested in-memory implementation — a tiny simulated screen
//! whose state actually changes as you drive it — used by tests, the supervisor's default bring-up,
//! and offline replay.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors from the control plane. [`StaleGeneration`](ControlError::StaleGeneration) is the
/// load-bearing one: it is how index-first tapping stays safe across a changing UI.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ControlError {
    /// An [`Action::Tap`] carried a `generation` that no longer matches the live UI tree.
    #[error("stale UI-tree generation: action carried {got} but the live tree is {expected}; re-observe before acting")]
    StaleGeneration { expected: u64, got: u64 },
    /// An [`Action::Tap`] named a node index outside the current tree.
    #[error("node index {index} out of range (tree has {len} nodes)")]
    NodeIndexOutOfRange { index: u32, len: usize },
    /// [`Action::Restore`] named a snapshot that was never saved.
    #[error("snapshot `{0}` not found")]
    SnapshotNotFound(String),
    /// The controller is not in a state that can serve observations/actions.
    #[error("controller not ready")]
    NotReady,
    /// A device backend command (e.g. `adb`, a launcher) failed. Carries the backend's message.
    /// Used by real controllers ([`aemulator-adb`](https://docs.rs/aemulator-adb)); the
    /// [`MockController`] never produces it.
    #[error("device backend error: {0}")]
    Backend(String),
}

/// A rectangle in device pixels (the device's own coordinate space, not the resized capture).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// The center point, used to synthesize a tap on this node's bounds.
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }
}

/// One node in the structured UI tree (sourced host-side from a platform-privileged, **non-
/// enumerable** component — never an app-registered AccessibilityService).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiNode {
    /// Stable index within this tree snapshot; the primary way actions name a node.
    pub index: u32,
    /// Visible text.
    pub text: String,
    /// `contentDescription`.
    pub content_desc: String,
    /// Widget class, e.g. `"android.widget.Button"`.
    pub class: String,
    /// On-screen bounds in device pixels.
    pub bounds: Rect,
    pub clickable: bool,
    pub focusable: bool,
}

/// A snapshot of the structured UI, tagged with the `generation` it was captured at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTree {
    pub nodes: Vec<UiNode>,
    pub generation: u64,
}

/// The full perception payload returned by [`DeviceController::observe`] and every mutating
/// [`DeviceController::act`]. `generation` matches `ui_tree.generation`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// Host-side framebuffer capture as PNG bytes (empty in the mock).
    pub screenshot_png: Vec<u8>,
    /// Capture width in pixels.
    pub width: u32,
    /// Capture height in pixels.
    pub height: u32,
    /// The structured UI tree captured at the same instant.
    pub ui_tree: UiTree,
    /// The generation counter this observation reflects.
    pub generation: u64,
}

/// The action set. Index-first ([`Tap`](Action::Tap)) is preferred; [`TapXy`](Action::TapXy) is the
/// explicit raw-coordinate escape hatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Action {
    /// Tap the node at `index`, valid only if `generation` still matches the live tree.
    Tap { index: u32, generation: u64 },
    /// Tap raw device coordinates (escape hatch — bypasses the generation guard).
    TapXy { x: i32, y: i32 },
    /// Swipe from `(x1,y1)` to `(x2,y2)` over `duration_ms`.
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: u32,
    },
    /// Inject text into the focused field.
    Text { text: String },
    /// Inject a key event (Android keycode).
    Key { code: i32 },
    /// Launch an app by package name.
    LaunchApp { pkg: String },
    /// Install an APK from a host path (adbd install path, gated off afterward).
    InstallApk { path: PathBuf },
    /// Set the simulated GPS fix (native IGnss — `Location.isMock()==false`).
    SetGps { lat: f64, lng: f64 },
    /// Block until the UI is perceptually stable (frame-hash + tree-hash + animation check).
    WaitStable,
    /// Save a named snapshot of device state.
    Snapshot { name: String },
    /// Restore a previously-saved named snapshot.
    Restore { name: String },
}

/// The control-plane contract. `observe` is read-only; `act` performs one action and returns the
/// resulting [`Observation`].
pub trait DeviceController {
    /// Capture the current perception payload.
    fn observe(&self) -> Result<Observation, ControlError>;
    /// Perform one [`Action`] and return the post-action observation.
    fn act(&mut self, action: Action) -> Result<Observation, ControlError>;
}

/// What tapping a node does, in the mock's little simulated screen. Attached per (screen, index)
/// out-of-band, since [`UiNode`] is a frozen wire type with no room for behaviour.
#[derive(Debug, Clone)]
enum NodeEffect {
    /// Navigate to the named screen (swaps the whole visible tree).
    Navigate(String),
    /// Flip the node's own `text` between two labels (a settings toggle).
    ToggleText { off: String, on: String },
    /// Append a fresh node to the current screen (a "+ add" affordance).
    AppendNode,
    /// Default: mark the node pressed in its `content_desc` so *every* tap visibly mutates state.
    Press,
}

/// One screen of the simulated device: its visible nodes plus the per-index tap behaviour.
#[derive(Debug, Clone, Default)]
struct Screen {
    nodes: Vec<UiNode>,
    effects: HashMap<u32, NodeEffect>,
}

/// The full mutable device state captured by [`Action::Snapshot`]. The `generation` is deliberately
/// **not** captured — restoring rewinds the *screens*, never the monotonic counter.
#[derive(Debug, Clone)]
struct DeviceSnapshot {
    current: String,
    screens: HashMap<String, Screen>,
    press_count: u64,
    note_count: u64,
}

/// A real, in-memory [`DeviceController`] used by tests, the supervisor's bring-up, and offline
/// replay. It models a tiny multi-screen app so taps genuinely mutate state (navigation, toggles,
/// node insertion) while the generation counter is tracked faithfully — the stale-generation guard
/// is therefore exercisable without a running guest.
#[derive(Debug, Clone)]
pub struct MockController {
    width: u32,
    height: u32,
    generation: u64,
    screens: HashMap<String, Screen>,
    current: String,
    snapshots: HashMap<String, DeviceSnapshot>,
    last_action: Option<Action>,
    press_count: u64,
    note_count: u64,
}

impl MockController {
    /// A mock at Anthropic's 720×1600 capture size, seeded with a tiny navigable app: a **home**
    /// screen (two nav buttons) reachable to a **settings** screen (a Wi-Fi toggle) and a **notes**
    /// screen (an "add note" affordance). Home shows two tappable nodes.
    pub fn new() -> Self {
        let (screens, current) = default_app();
        Self {
            width: 720,
            height: 1600,
            generation: 1,
            screens,
            current,
            snapshots: HashMap::new(),
            last_action: None,
            press_count: 0,
            note_count: 0,
        }
    }

    /// A mock with an explicit capture size and node set. The nodes become a single `"custom"`
    /// screen with no navigation, so tapping an in-range node marks it pressed (in `content_desc`)
    /// — every valid tap still mutates state and bumps the generation.
    pub fn with_nodes(width: u32, height: u32, nodes: Vec<UiNode>) -> Self {
        let mut screens = HashMap::new();
        screens.insert(
            "custom".to_string(),
            Screen {
                nodes,
                effects: HashMap::new(),
            },
        );
        Self {
            width,
            height,
            generation: 1,
            screens,
            current: "custom".to_string(),
            snapshots: HashMap::new(),
            last_action: None,
            press_count: 0,
            note_count: 0,
        }
    }

    /// The last action performed (for replay/assertions).
    pub fn last_action(&self) -> Option<&Action> {
        self.last_action.as_ref()
    }

    /// The current generation counter.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The name of the currently-displayed screen (`"home"`/`"settings"`/`"notes"` for the default
    /// app, `"custom"` for a [`with_nodes`](Self::with_nodes) controller). Introspection aid.
    pub fn current_screen(&self) -> &str {
        &self.current
    }

    /// Stub for the host-side `wait_for_stable` primitive. A real controller blocks on a perceptual
    /// frame-hash + a11y-tree hash + animation check before returning; the mock's UI is synchronous
    /// (never mid-animation), so it is always stable immediately and this simply re-observes without
    /// advancing the generation.
    pub fn wait_stable(&self) -> Result<Observation, ControlError> {
        self.observe()
    }

    /// Borrow the current screen, or [`ControlError::NotReady`] if it is somehow absent (an
    /// invariant violation — every constructor inserts the current screen).
    fn current_screen_ref(&self) -> Result<&Screen, ControlError> {
        self.screens
            .get(&self.current)
            .ok_or(ControlError::NotReady)
    }

    fn current_observation(&self) -> Result<Observation, ControlError> {
        let nodes = self.current_screen_ref()?.nodes.clone();
        Ok(Observation {
            screenshot_png: Vec::new(),
            width: self.width,
            height: self.height,
            ui_tree: UiTree {
                nodes,
                generation: self.generation,
            },
            generation: self.generation,
        })
    }

    /// Apply the effect of tapping node `index` on the current screen. The index is validated by the
    /// caller; this only mutates state.
    fn apply_tap(&mut self, index: u32) -> Result<(), ControlError> {
        // Clone the effect out so the immutable borrow ends before we mutate.
        let effect = self
            .current_screen_ref()?
            .effects
            .get(&index)
            .cloned()
            .unwrap_or(NodeEffect::Press);

        match effect {
            NodeEffect::Navigate(target) => {
                if !self.screens.contains_key(&target) {
                    return Err(ControlError::NotReady);
                }
                self.current = target;
            }
            NodeEffect::ToggleText { off, on } => {
                let screen = self
                    .screens
                    .get_mut(&self.current)
                    .ok_or(ControlError::NotReady)?;
                if let Some(node) = screen.nodes.get_mut(index as usize) {
                    node.text = if node.text == on { off } else { on };
                }
            }
            NodeEffect::AppendNode => {
                self.note_count += 1;
                let label = format!("Note {}", self.note_count);
                let screen = self
                    .screens
                    .get_mut(&self.current)
                    .ok_or(ControlError::NotReady)?;
                let new_index = screen.nodes.len() as u32;
                let y = 520 + (new_index as i32) * 140;
                screen.nodes.push(UiNode {
                    index: new_index,
                    text: label,
                    content_desc: String::new(),
                    class: "android.widget.TextView".to_string(),
                    bounds: Rect {
                        x: 40,
                        y,
                        width: 640,
                        height: 120,
                    },
                    clickable: false,
                    focusable: false,
                });
            }
            NodeEffect::Press => {
                self.press_count += 1;
                let marker = format!("pressed:{}", self.press_count);
                let screen = self
                    .screens
                    .get_mut(&self.current)
                    .ok_or(ControlError::NotReady)?;
                if let Some(node) = screen.nodes.get_mut(index as usize) {
                    node.content_desc = marker;
                }
            }
        }
        Ok(())
    }

    fn snapshot_now(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            current: self.current.clone(),
            screens: self.screens.clone(),
            press_count: self.press_count,
            note_count: self.note_count,
        }
    }

    fn restore_from(&mut self, snap: DeviceSnapshot) {
        self.current = snap.current;
        self.screens = snap.screens;
        self.press_count = snap.press_count;
        self.note_count = snap.note_count;
    }
}

impl Default for MockController {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceController for MockController {
    fn observe(&self) -> Result<Observation, ControlError> {
        self.current_observation()
    }

    fn act(&mut self, action: Action) -> Result<Observation, ControlError> {
        match &action {
            Action::Tap { index, generation } => {
                // Generation guard first: a stale tap must fail loud and NOT mutate state.
                if *generation != self.generation {
                    return Err(ControlError::StaleGeneration {
                        expected: self.generation,
                        got: *generation,
                    });
                }
                let len = self.current_screen_ref()?.nodes.len();
                if (*index as usize) >= len {
                    return Err(ControlError::NodeIndexOutOfRange { index: *index, len });
                }
                // Valid tap: mutate the simulated screen, then advance the generation.
                self.apply_tap(*index)?;
                self.generation += 1;
            }
            Action::WaitStable => {
                // No state change → generation stays put (the UI is, by definition, stable).
            }
            Action::Snapshot { name } => {
                // Capturing state is not itself a visible UI change → generation is unchanged.
                let snap = self.snapshot_now();
                self.snapshots.insert(name.clone(), snap);
            }
            Action::Restore { name } => {
                let snap = self
                    .snapshots
                    .get(name)
                    .cloned()
                    .ok_or_else(|| ControlError::SnapshotNotFound(name.clone()))?;
                self.restore_from(snap);
                // The visible tree changed, and the counter is monotonic → advance (never rewind).
                self.generation += 1;
            }
            _ => {
                // Every other mutating action (TapXy/Swipe/Text/Key/LaunchApp/InstallApk/SetGps)
                // advances the generation so its post-action observation is fresh.
                self.generation += 1;
            }
        }
        self.last_action = Some(action);
        self.current_observation()
    }
}

/// Build the default little app: `home` → {`settings`, `notes`}. Returns the screen map and the
/// initial current-screen name (`"home"`).
fn default_app() -> (HashMap<String, Screen>, String) {
    fn button(index: u32, text: &str, y: i32) -> UiNode {
        UiNode {
            index,
            text: text.to_string(),
            content_desc: String::new(),
            class: "android.widget.Button".to_string(),
            bounds: Rect {
                x: 40,
                y,
                width: 640,
                height: 120,
            },
            clickable: true,
            focusable: true,
        }
    }

    let mut home_effects = HashMap::new();
    home_effects.insert(0, NodeEffect::Navigate("settings".to_string()));
    home_effects.insert(1, NodeEffect::Navigate("notes".to_string()));
    let home = Screen {
        nodes: vec![
            button(0, "Open Settings", 200),
            button(1, "Open Notes", 360),
        ],
        effects: home_effects,
    };

    let mut settings_effects = HashMap::new();
    settings_effects.insert(0, NodeEffect::Navigate("home".to_string()));
    settings_effects.insert(
        1,
        NodeEffect::ToggleText {
            off: "Wi-Fi: Off".to_string(),
            on: "Wi-Fi: On".to_string(),
        },
    );
    let settings = Screen {
        nodes: vec![button(0, "Back", 200), button(1, "Wi-Fi: Off", 360)],
        effects: settings_effects,
    };

    let mut notes_effects = HashMap::new();
    notes_effects.insert(0, NodeEffect::Navigate("home".to_string()));
    notes_effects.insert(1, NodeEffect::AppendNode);
    let notes = Screen {
        nodes: vec![button(0, "Back", 200), button(1, "Add note", 360)],
        effects: notes_effects,
    };

    let mut screens = HashMap::new();
    screens.insert("home".to_string(), home);
    screens.insert("settings".to_string(), settings);
    screens.insert("notes".to_string(), notes);
    (screens, "home".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_reports_seeded_tree_and_generation() {
        let c = MockController::new();
        let obs = c.observe().unwrap();
        assert_eq!(obs.width, 720);
        assert_eq!(obs.height, 1600);
        assert_eq!(obs.generation, 1);
        assert_eq!(obs.ui_tree.generation, 1);
        assert_eq!(obs.ui_tree.nodes.len(), 2);
        assert_eq!(c.current_screen(), "home");
    }

    #[test]
    fn observe_is_side_effect_free() {
        let c = MockController::new();
        let g = c.observe().unwrap().generation;
        // Observing repeatedly must never advance the generation.
        for _ in 0..5 {
            assert_eq!(c.observe().unwrap().generation, g);
        }
    }

    #[test]
    fn tap_with_current_generation_succeeds_and_advances() {
        let mut c = MockController::new();
        let g = c.observe().unwrap().generation;
        let obs = c
            .act(Action::Tap {
                index: 0,
                generation: g,
            })
            .unwrap();
        assert_eq!(obs.generation, g + 1);
    }

    #[test]
    fn valid_tap_mutates_state() {
        // Design invariant: a valid tap on an index changes the observable device state, not just
        // the counter. Home node 0 navigates to the settings screen.
        let mut c = MockController::new();
        let before = c.observe().unwrap();
        let g = before.generation;
        let after = c
            .act(Action::Tap {
                index: 0,
                generation: g,
            })
            .unwrap();

        assert_eq!(c.current_screen(), "settings");
        assert_eq!(after.generation, g + 1);
        // The tree genuinely changed.
        assert_ne!(after.ui_tree.nodes, before.ui_tree.nodes);
        assert_eq!(after.ui_tree.nodes[1].text, "Wi-Fi: Off");
    }

    #[test]
    fn navigation_is_round_trippable() {
        let mut c = MockController::new();
        // home → settings → back to home.
        let g = c.generation();
        c.act(Action::Tap {
            index: 0,
            generation: g,
        })
        .unwrap();
        assert_eq!(c.current_screen(), "settings");
        let g = c.generation();
        c.act(Action::Tap {
            index: 0,
            generation: g,
        })
        .unwrap();
        assert_eq!(c.current_screen(), "home");
    }

    #[test]
    fn toggle_node_flips_its_own_text() {
        let mut c = MockController::new();
        // Go to settings.
        let g = c.generation();
        c.act(Action::Tap {
            index: 0,
            generation: g,
        })
        .unwrap();
        // Toggle Wi-Fi on.
        let g = c.generation();
        let on = c
            .act(Action::Tap {
                index: 1,
                generation: g,
            })
            .unwrap();
        assert_eq!(on.ui_tree.nodes[1].text, "Wi-Fi: On");
        // Toggle it back off.
        let g = c.generation();
        let off = c
            .act(Action::Tap {
                index: 1,
                generation: g,
            })
            .unwrap();
        assert_eq!(off.ui_tree.nodes[1].text, "Wi-Fi: Off");
    }

    #[test]
    fn add_note_appends_a_node() {
        let mut c = MockController::new();
        // home → notes.
        let g = c.generation();
        c.act(Action::Tap {
            index: 1,
            generation: g,
        })
        .unwrap();
        assert_eq!(c.current_screen(), "notes");
        let before = c.observe().unwrap().ui_tree.nodes.len();
        let g = c.generation();
        let after = c
            .act(Action::Tap {
                index: 1,
                generation: g,
            })
            .unwrap();
        assert_eq!(after.ui_tree.nodes.len(), before + 1);
        assert_eq!(after.ui_tree.nodes.last().unwrap().text, "Note 1");
    }

    #[test]
    fn press_fallback_marks_custom_nodes() {
        // A with_nodes controller has no navigation; a valid tap marks the node pressed, so state
        // still mutates for arbitrary injected node sets.
        let nodes = vec![UiNode {
            index: 0,
            text: "Widget".to_string(),
            content_desc: String::new(),
            class: "android.widget.TextView".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            clickable: true,
            focusable: false,
        }];
        let mut c = MockController::with_nodes(1080, 2340, nodes);
        assert_eq!(c.observe().unwrap().width, 1080);
        let g = c.generation();
        let obs = c
            .act(Action::Tap {
                index: 0,
                generation: g,
            })
            .unwrap();
        assert_eq!(obs.ui_tree.nodes[0].content_desc, "pressed:1");
        assert_eq!(obs.generation, g + 1);
    }

    #[test]
    fn tap_with_stale_generation_fails_loud() {
        let mut c = MockController::new();
        let g = c.observe().unwrap().generation;
        let err = c
            .act(Action::Tap {
                index: 0,
                generation: g - 1,
            })
            .unwrap_err();
        assert_eq!(
            err,
            ControlError::StaleGeneration {
                expected: g,
                got: g - 1
            }
        );
        // A failed action must NOT advance the generation or change the screen.
        assert_eq!(c.generation(), g);
        assert_eq!(c.current_screen(), "home");
    }

    #[test]
    fn stale_generation_does_not_mutate_state() {
        // Tighter form of the guard: capture the full tree, attempt a stale tap that WOULD have
        // navigated, and confirm the observable tree is byte-identical afterwards.
        let mut c = MockController::new();
        let before = c.observe().unwrap();
        let stale = before.generation + 7;
        let err = c
            .act(Action::Tap {
                index: 0,
                generation: stale,
            })
            .unwrap_err();
        assert!(matches!(err, ControlError::StaleGeneration { .. }));
        let after = c.observe().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn tap_out_of_range_is_rejected() {
        let mut c = MockController::new();
        let g = c.generation();
        let err = c
            .act(Action::Tap {
                index: 99,
                generation: g,
            })
            .unwrap_err();
        assert_eq!(err, ControlError::NodeIndexOutOfRange { index: 99, len: 2 });
        // Out-of-range is checked AFTER the generation guard, and still must not mutate.
        assert_eq!(c.generation(), g);
    }

    #[test]
    fn every_mutating_action_returns_a_fresh_observation() {
        // Each of these advances the generation, so its returned observation is strictly newer.
        let mutating = vec![
            Action::TapXy { x: 10, y: 20 },
            Action::Swipe {
                x1: 0,
                y1: 0,
                x2: 100,
                y2: 100,
                duration_ms: 250,
            },
            Action::Text {
                text: "hello".to_string(),
            },
            Action::Key { code: 4 },
            Action::LaunchApp {
                pkg: "com.example.app".to_string(),
            },
            Action::InstallApk {
                path: PathBuf::from("/tmp/app.apk"),
            },
            Action::SetGps {
                lat: 37.7749,
                lng: -122.4194,
            },
        ];
        let mut c = MockController::new();
        for action in mutating {
            let before = c.generation();
            let obs = c.act(action.clone()).unwrap();
            assert_eq!(
                obs.generation,
                before + 1,
                "action {action:?} must bump generation"
            );
            assert_eq!(obs.generation, c.generation());
            assert_eq!(c.last_action(), Some(&action));
        }
    }

    #[test]
    fn wait_stable_action_does_not_advance_generation() {
        let mut c = MockController::new();
        let g = c.generation();
        let obs = c.act(Action::WaitStable).unwrap();
        assert_eq!(obs.generation, g);
        assert_eq!(c.generation(), g);
    }

    #[test]
    fn wait_stable_method_stub_reobserves_without_mutating() {
        let c = MockController::new();
        let g = c.generation();
        let obs = c.wait_stable().unwrap();
        assert_eq!(obs.generation, g);
        assert_eq!(c.generation(), g);
    }

    #[test]
    fn snapshot_does_not_advance_generation() {
        let mut c = MockController::new();
        let g = c.generation();
        c.act(Action::Snapshot {
            name: "s".to_string(),
        })
        .unwrap();
        assert_eq!(c.generation(), g);
    }

    #[test]
    fn snapshot_and_restore_roundtrip() {
        let mut c = MockController::with_nodes(720, 1600, vec![]);
        c.act(Action::Snapshot {
            name: "home".to_string(),
        })
        .unwrap();
        // Mutate away from the snapshot.
        c.act(Action::TapXy { x: 1, y: 1 }).unwrap();
        let restored = c
            .act(Action::Restore {
                name: "home".to_string(),
            })
            .unwrap();
        assert!(restored.ui_tree.nodes.is_empty());
    }

    #[test]
    fn restore_rewinds_screen_but_generation_stays_monotonic() {
        let mut c = MockController::new();
        // Snapshot on home, then navigate away to settings.
        c.act(Action::Snapshot {
            name: "start".to_string(),
        })
        .unwrap();
        let g = c.generation();
        c.act(Action::Tap {
            index: 0,
            generation: g,
        })
        .unwrap();
        assert_eq!(c.current_screen(), "settings");
        let g_before_restore = c.generation();

        let restored = c
            .act(Action::Restore {
                name: "start".to_string(),
            })
            .unwrap();
        // Screen is back to home ...
        assert_eq!(c.current_screen(), "home");
        // ... but the generation moved forward, never backward.
        assert!(restored.generation > g_before_restore);
        assert_eq!(restored.generation, g_before_restore + 1);
    }

    #[test]
    fn restore_unknown_snapshot_errors() {
        let mut c = MockController::new();
        let err = c
            .act(Action::Restore {
                name: "missing".to_string(),
            })
            .unwrap_err();
        assert_eq!(err, ControlError::SnapshotNotFound("missing".to_string()));
    }

    #[test]
    fn rect_center_is_the_midpoint() {
        let r = Rect {
            x: 40,
            y: 200,
            width: 640,
            height: 120,
        };
        assert_eq!(r.center(), (360, 260));
    }

    #[test]
    fn observation_serde_roundtrips() {
        let c = MockController::new();
        let obs = c.observe().unwrap();
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(obs, back);
    }
}
