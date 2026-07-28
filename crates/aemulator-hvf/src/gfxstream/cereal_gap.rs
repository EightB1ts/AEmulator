//! `cereal_gap` — compute the set of Vulkan extensions gfxstream's generated cereal code can
//! **size** but cannot **unmarshal**, so the host renderer never advertises them to the guest.
//!
//! # The bug this exists to make impossible
//! gfxstream's guest→host Vulkan wire format is generated code. Two of those generated switches
//! disagree:
//!
//! * `goldfish_vk_extension_struct_size` (`host/vulkan/cereal/common/goldfish_vk_extension_structs.cpp`)
//!   knows the byte size of 524 `VkStructureType`s.
//! * `reservedunmarshal_extension_struct` (`.../goldfish_vk_reserved_marshaling.cpp`) knows how to
//!   *decode* only 195 of them. Its `default:` arm is a literal `abort()` — of the whole VMM.
//!
//! So any guest that enables an extension in the 329-type difference and chains one of its structs
//! into a `pNext` list kills the emulator. `VK_EXT_memory_budget` did exactly that; the fix at the
//! time was a hand-curated 9-entry strip list, which is a subset of a 166-extension problem.
//!
//! # Why the fix is "keep it off the wire", not "skip the struct"
//! Softening the `abort()` into a skip is **not** an option and must not be attempted. The wire
//! carries a single `pNext_size` for the whole chain, not a per-struct body length, and the bodies
//! are not fixed size — a decoder that does not know a struct cannot know how many bytes to step
//! over, so skipping desynchronises the stream for every later field. Filtering the extension out
//! of `vkEnumerateDeviceExtensionProperties` means the guest never enables it, so the struct is
//! never put on the wire at all. No wire-format guessing is involved.
//!
//! # What this module is
//! A pure, dependency-free parser over the two generated `.cpp` files as **text**, plus an emitter
//! for `host/vulkan/VkDecoderCerealGap.inc` — the generated strip list that
//! `VkDecoderGlobalState.cpp` includes. `src/bin/aemu-cereal-gap.rs` is the CLI around it and
//! `ci/hvf-doctor.sh` runs its `verify` mode as a drift guard, so a gfxstream pin bump that changes
//! the codegen fails at doctor time instead of at `abort()` time.
//!
//! Parsing text rather than running the upstream Python codegen is deliberate: the codegen needs a
//! `vk.xml` registry snapshot we do not vendor, and what actually matters is the gap in **the files
//! that get compiled**, which is exactly what is read here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// The size-table source, relative to a gfxstream checkout root.
pub const SIZE_TABLE_REL_PATH: &str = "host/vulkan/cereal/common/goldfish_vk_extension_structs.cpp";
/// The reserved-unmarshal source, relative to a gfxstream checkout root.
pub const UNMARSHAL_REL_PATH: &str =
    "host/vulkan/cereal/common/goldfish_vk_reserved_marshaling.cpp";
/// The generated include this module emits, relative to a gfxstream checkout root.
pub const INC_REL_PATH: &str = "host/vulkan/VkDecoderCerealGap.inc";

/// The size switch. Note this is *not* `..._with_stream_features`: the two are byte-identical in
/// their case sets, and matching the shorter name is unambiguous because the function-definition
/// scanner requires the very next non-space character after the name to be `(`.
const SIZE_TABLE_FN: &str = "goldfish_vk_extension_struct_size";
/// The switch whose `default:` arm is the `abort()`.
const UNMARSHAL_FN: &str = "reservedunmarshal_extension_struct";

/// Not an extension — a build-configuration guard the codegen nests *inside* the extension guard.
/// It must never be mistaken for the owning extension of a case label.
const BETA_GUARD: &str = "VK_ENABLE_BETA_EXTENSIONS";

/// Registered Vulkan author IDs, used only to recognise the vendor tag of a name. Kept explicit
/// rather than "any run of capitals" so a struct name like `..._CREATE_INFO` can never be read as
/// a vendor.
#[rustfmt::skip]
const VENDOR_TAGS: &[&str] = &[
    "AMD", "AMDX", "ANDROID", "ARM", "BRCM", "EXT", "EXTX", "FSL", "FUCHSIA", "GGP", "GOOGLE",
    "HUAWEI", "IMG", "INTEL", "JUICE", "KHR", "KHX", "LUNARG", "MESA", "MSFT", "MVK", "NN", "NV",
    "NVX", "NXP", "QCOM", "QNX", "RENDERDOC", "SEC", "VALVE", "VSI",
];

/// One extension that the size table knows and the unmarshal switch does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapEntry {
    /// The extension name, taken from the `#ifdef` guard the codegen wraps its cases in.
    pub extension: String,
    /// Every `VK_STRUCTURE_TYPE_*` this extension contributes to the size table, sorted.
    pub structs: Vec<String>,
}

/// Parse the `goldfish_vk_extension_struct_size` switch and the
/// `reservedunmarshal_extension_struct` switch; return extensions with >=1 `VkStructureType` in the
/// former and none in the latter, sorted by extension name.
///
/// Extensions are identified by the `#ifdef VK_<extension_suffix>` guard the codegen wraps each
/// group of cases in — that is the only signal in these files that maps a `VkStructureType` back to
/// its owner, and it is exactly what the upstream generator emits.
///
/// `#ifdef VK_VERSION_*` blocks are skipped: core versions are not extensions, cannot be stripped
/// from an enumerate result, and their structs are all handled anyway.
pub fn compute_gap(size_table_src: &str, unmarshal_src: &str) -> Vec<GapEntry> {
    let sized = cases_by_extension(size_table_src, SIZE_TABLE_FN);
    let unmarshalled = all_case_labels(unmarshal_src, UNMARSHAL_FN);
    sized
        .into_iter()
        .filter(|(_, structs)| {
            // "none in the latter" is tested per struct, not per guard, so an extension whose
            // structs the unmarshal switch happens to file under a *different* guard is correctly
            // reported as handled.
            !structs.is_empty() && structs.iter().all(|s| !unmarshalled.contains(s))
        })
        .map(|(extension, structs)| GapEntry {
            extension,
            structs: structs.into_iter().collect(),
        })
        .collect()
}

/// The promoted spelling of `extension`, when the codegen filed the extension's structs under
/// their *promoted* names only.
///
/// This is not cosmetic. `VK_VALVE_mutable_descriptor_type` guards two cases that are both spelled
/// `..._EXT`, because `VK_EXT_mutable_descriptor_type` is its promotion and the two extensions
/// share one set of `VkStructureType` values. The host driver advertises the `EXT` name, so a strip
/// list built from guard names alone would hide a name nothing advertises and leave the aborting
/// struct reachable. Returns `None` unless every contributing struct carries the *same* vendor tag
/// and that tag differs from the guard's own — a mixed set means the guard collects structs from
/// several extensions (`VK_KHR_dynamic_rendering` does) and no alias can be derived.
///
/// A wrong guess here is harmless (an extension name no driver reports is dead weight in the strip
/// list); a missing one is an `abort()`, hence the deliberate bias toward emitting it.
pub fn promotion_alias(extension: &str, structs: &[String]) -> Option<String> {
    let (vendor, suffix) = extension.strip_prefix("VK_")?.split_once('_')?;
    if !VENDOR_TAGS.contains(&vendor) {
        return None;
    }
    let mut tag: Option<&str> = None;
    for s in structs {
        let t = s.rsplit('_').next()?;
        if !VENDOR_TAGS.contains(&t) {
            return None;
        }
        match tag {
            None => tag = Some(t),
            Some(prev) if prev == t => {}
            Some(_) => return None,
        }
    }
    let tag = tag?;
    if tag == vendor {
        return None;
    }
    Some(format!("VK_{tag}_{suffix}"))
}

/// Render `host/vulkan/VkDecoderCerealGap.inc` — the generated, `#include`-able strip list plus the
/// struct→extension table the improved `reservedunmarshal_extension_struct` abort message uses.
///
/// Deterministic: `generate` and `verify` must produce byte-identical output for the drift guard to
/// mean anything.
pub fn render_inc(gap: &[GapEntry], gfxstream_sha: &str) -> String {
    // extension name -> comment lines (first trailing, rest continued). A promotion alias is
    // emitted as its own entry, in sorted position, so the C++ side needs no alias logic of its own.
    let mut entries: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut struct_owner: BTreeMap<String, String> = BTreeMap::new();
    for e in gap {
        entries.insert(e.extension.clone(), e.structs.clone());
        if let Some(alias) = promotion_alias(&e.extension, &e.structs) {
            entries.entry(alias).or_insert_with(|| {
                vec![format!(
                    "promoted spelling of {} — same structs",
                    e.extension
                )]
            });
        }
        for s in &e.structs {
            struct_owner.insert(s.clone(), e.extension.clone());
        }
    }

    let mut out = String::new();
    let _ = write!(
        out,
        r#"// VkDecoderCerealGap.inc — GENERATED FILE, DO NOT EDIT.
//
// Vulkan device extensions that MUST NOT be advertised to the guest, because this gfxstream tree's
// generated cereal code can size their pNext structs but cannot decode them.
//
// Every name below owns at least one VkStructureType that
//   goldfish_vk_extension_struct_size()      (cereal/common/goldfish_vk_extension_structs.cpp)
// returns a size for and that
//   reservedunmarshal_extension_struct()     (cereal/common/goldfish_vk_reserved_marshaling.cpp)
// has no case for. That switch's default arm is abort() — of the entire VMM. A guest that enables
// the extension and chains the struct kills the emulator, so the extension is filtered out of
// vkEnumerateDeviceExtensionProperties and the struct never reaches the wire.
//
// Do NOT "fix" this by softening the abort() into a skip: the wire carries one pNext_size for the
// whole chain, not a per-struct body length, and the bodies are not fixed size. A decoder that
// does not know a struct cannot know how many bytes to step over. Skipping desynchronises the
// stream, which is strictly worse than aborting.
//
//   gfxstream pin : {sha}
//   extensions    : {n_ext} ({n_alias} of them promoted spellings of a guard name)
//   struct types  : {n_struct}
//
// REGENERATE (do not hand-edit):
//   cargo run --manifest-path crates/aemulator-hvf/Cargo.toml --bin aemu-cereal-gap -- \
//       generate third_party/gfxstream > third_party/gfxstream/host/vulkan/VkDecoderCerealGap.inc
// then re-export the patch series (third_party/patches/gfxstream/).
//
// DRIFT GUARD: ci/hvf-doctor.sh runs `aemu-cereal-gap verify`, which fails if this file no longer
// matches what the pinned tree's codegen implies. A pin bump that changes the codegen therefore
// fails at doctor time instead of at abort() time, mid-boot.

#ifndef AEMULATOR_VK_DECODER_CEREAL_GAP_INC
#define AEMULATOR_VK_DECODER_CEREAL_GAP_INC

// NO #include DIRECTIVES IN THIS FILE, deliberately. VkDecoderGlobalState.cpp includes it at
// NAMESPACE scope (in place of the hand-written array it replaces), and a system header expanded
// inside `namespace gfxstream::vk` is ill-formed — it would happen to compile only for as long as
// something else had already included the same header first. The one thing this file needs from the
// outside is strcmp, i.e. <string.h> or <cstring>; both includers pull it in well before this point.
//
// Its own namespace and internal linkage, because it is included by two translation units:
// VkDecoderGlobalState.cpp (uses kCerealGapExtensions) and
// cereal/common/goldfish_vk_reserved_marshaling.cpp (uses CerealGapExtensionForStructType to name
// the owning extension in its abort message). [[maybe_unused]] because each of those needs only one
// of the two tables.
namespace aemu_cereal_gap {{

// The strip list. Trailing comment names the contributing VkStructureType(s), one per line.
[[maybe_unused]] static constexpr const char* const kCerealGapExtensions[] = {{
"#,
        sha = gfxstream_sha,
        n_ext = entries.len(),
        n_alias = entries.len() - gap.len(),
        n_struct = struct_owner.len(),
    );

    for (name, comment) in &entries {
        match comment.split_first() {
            Some((first, rest)) => {
                let _ = writeln!(out, "    \"{name}\",  // {first}");
                for line in rest {
                    let _ = writeln!(out, "        // {line}");
                }
            }
            None => {
                let _ = writeln!(out, "    \"{name}\",");
            }
        }
    }

    out.push_str(
        r#"};

// VkStructureType name -> the extension that owns it, for the reservedunmarshal abort message.
// Keyed by the string_VkStructureType() spelling so the lookup needs no enum values.
struct CerealGapStructToExtension {
    const char* structType;
    const char* extension;
};

[[maybe_unused]] static constexpr CerealGapStructToExtension kCerealGapStructToExtension[] = {
"#,
    );

    for (s, owner) in &struct_owner {
        let _ = writeln!(out, "    {{\"{s}\", \"{owner}\"}},");
    }

    out.push_str(
        r#"};

// Linear scan on purpose: this runs once, on the path that is about to abort() the process.
[[maybe_unused]] static const char* CerealGapExtensionForStructType(const char* structTypeName) {
    if (!structTypeName) return nullptr;
    for (const auto& entry : kCerealGapStructToExtension) {
        if (strcmp(entry.structType, structTypeName) == 0) return entry.extension;
    }
    return nullptr;
}

}  // namespace aemu_cereal_gap

#endif  // AEMULATOR_VK_DECODER_CEREAL_GAP_INC
"#,
    );
    out
}

// ---------------------------------------------------------------------------------------------
// Parsing internals. Everything below works on the generated files as plain text.
// ---------------------------------------------------------------------------------------------

/// Every `VK_STRUCTURE_TYPE_*` case label in `fn_name`'s switch, grouped by the extension its
/// `#ifdef` guard names. Core-version and unguarded cases are dropped.
fn cases_by_extension(src: &str, fn_name: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (owner, label) in walk_cases(&switch_body(src, fn_name)) {
        if let Some(ext) = owner {
            out.entry(ext).or_default().insert(label);
        }
    }
    out
}

/// Every `VK_STRUCTURE_TYPE_*` case label in `fn_name`'s switch, regardless of guard.
fn all_case_labels(src: &str, fn_name: &str) -> BTreeSet<String> {
    walk_cases(&switch_body(src, fn_name))
        .into_iter()
        .map(|(_, label)| label)
        .collect()
}

/// The body lines of `fn_name`'s **definition** (not a forward declaration), between the opening
/// brace and the first `}` in column zero. Empty if no definition is found.
fn switch_body<'a>(src: &'a str, fn_name: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = src.lines().collect();
    for (i, &line) in lines.iter().enumerate() {
        let Some(open) = fn_name_paren(line, fn_name) else {
            continue;
        };
        let Some((close_line, after_close)) = balanced_paren_end(&lines, i, open) else {
            continue;
        };
        // A declaration ends in `;`. Only a definition has a brace here, which is what separates
        // `void reservedunmarshal_extension_struct(...);` near the top of the file from the real
        // one 14k lines later.
        let Some(brace) = brace_line(&lines, close_line, after_close) else {
            continue;
        };
        // The generated files always close a function with `}` in column zero.
        if let Some(end) = (brace + 1..lines.len()).find(|&j| lines[j] == "}") {
            return lines[brace + 1..end].to_vec();
        }
    }
    Vec::new()
}

/// Byte index of the `(` that opens `name`'s parameter list on `line`, if `name` occurs there as a
/// whole identifier immediately followed by `(`.
///
/// The "immediately followed by `(`" rule is what keeps `goldfish_vk_extension_struct_size` from
/// matching `goldfish_vk_extension_struct_size_with_stream_features`.
fn fn_name_paren(line: &str, name: &str) -> Option<usize> {
    let hay = line.as_bytes();
    let needle = name.as_bytes();
    for (at, window) in hay.windows(needle.len()).enumerate() {
        if window != needle {
            continue;
        }
        if at > 0 && is_ident_byte(hay[at - 1]) {
            continue;
        }
        let mut k = at + needle.len();
        while k < hay.len() && hay[k] == b' ' {
            k += 1;
        }
        if k < hay.len() && hay[k] == b'(' {
            return Some(k);
        }
    }
    None
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `(line index, byte index just past the `)`)` that closes the parameter list opened at
/// `lines[start_line][open_at]`.
fn balanced_paren_end(lines: &[&str], start_line: usize, open_at: usize) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    for (offset, &line) in lines[start_line..].iter().enumerate() {
        let base = if offset == 0 { open_at } else { 0 };
        for (k, ch) in line[base..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((start_line + offset, base + k + 1));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Index of the line holding the `{` that opens the body, if the next non-whitespace character
/// after the closing `)` is one. `None` for a declaration (`;`) or an attribute we do not model.
fn brace_line(lines: &[&str], close_line: usize, after_close: usize) -> Option<usize> {
    let rest = lines[close_line][after_close..].trim_start();
    if let Some(c) = rest.chars().next() {
        return if c == '{' { Some(close_line) } else { None };
    }
    for (offset, &line) in lines[close_line + 1..].iter().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        return if t.starts_with('{') {
            Some(close_line + 1 + offset)
        } else {
            None
        };
    }
    None
}

/// Walk switch-body lines, tracking the `#ifdef` nesting, and yield
/// `(owning extension, case label)` for every `VK_STRUCTURE_TYPE_*` case. The owner is `None` for
/// a core-version block, an unguarded case, or a guard shape we do not attribute.
fn walk_cases(body: &[&str]) -> Vec<(Option<String>, String)> {
    // `None` for conditionals that name no extension (`#if`, `#ifndef`), so nesting stays balanced.
    let mut guards: Vec<Option<String>> = Vec::new();
    let mut out = Vec::new();
    for line in body {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("#ifdef") {
            guards.push(rest.split_whitespace().next().map(str::to_string));
        } else if t.starts_with("#if") {
            guards.push(None);
        } else if t.starts_with("#endif") {
            guards.pop();
        } else if let Some(label) = case_label(t) {
            out.push((owning_extension(&guards), label));
        }
    }
    out
}

fn case_label(trimmed: &str) -> Option<String> {
    let name = trimmed.strip_prefix("case ")?.split(':').next()?.trim();
    if name.starts_with("VK_STRUCTURE_TYPE_") && name.bytes().all(is_ident_byte) {
        Some(name.to_string())
    } else {
        None
    }
}

/// The extension that owns a case under `guards`, scanning outermost first — the codegen always
/// puts the extension guard outside build-configuration guards such as `VK_ENABLE_BETA_EXTENSIONS`.
fn owning_extension(guards: &[Option<String>]) -> Option<String> {
    for g in guards.iter().flatten() {
        if g == BETA_GUARD || !g.starts_with("VK_") {
            continue;
        }
        // A core version is not an extension: it cannot be removed from an enumerate result, so
        // reporting it would produce an entry nothing can act on.
        if g.starts_with("VK_VERSION_") {
            return None;
        }
        return Some(g.clone());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal stand-ins for the two generated files. Both reproduce the three shapes that matter:
    /// a forward declaration before the definition, `#ifdef` guards around groups of cases, and a
    /// `}` in column zero closing the function.
    fn size_src(body: &str) -> String {
        format!(
            "void goldfish_vk_extension_struct_size(VkStructureType rootType, const void* e);\n\
             \n\
             size_t goldfish_vk_extension_struct_size(VkStructureType rootType, const void* e) {{\n\
             \x20   switch (structType) {{\n{body}        default: {{\n            abort();\n        }}\n    }}\n}}\n"
        )
    }

    fn unmarshal_src(body: &str) -> String {
        format!(
            "void reservedunmarshal_extension_struct(VulkanStream* s, VkStructureType r, void* o);\n\
             \n\
             void reservedunmarshal_extension_struct(VulkanStream* s, VkStructureType r,\n\
             \x20                                       void* o, uint8_t** ptr) {{\n\
             \x20   switch (structType) {{\n{body}        default: {{\n            abort();\n        }}\n    }}\n}}\n"
        )
    }

    #[test]
    fn gap_flags_a_struct_in_the_size_table_and_absent_from_unmarshal() {
        let size = size_src(
            "#ifdef VK_EXT_extended_dynamic_state3\n\
             \x20       case VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_3_FEATURES_EXT: {\n\
             \x20           return sizeof(VkPhysicalDeviceExtendedDynamicState3FeaturesEXT);\n\
             \x20       }\n\
             #endif\n\
             #ifdef VK_KHR_swapchain\n\
             \x20       case VK_STRUCTURE_TYPE_IMAGE_SWAPCHAIN_CREATE_INFO_KHR: {\n\
             \x20           return sizeof(VkImageSwapchainCreateInfoKHR);\n\
             \x20       }\n\
             #endif\n",
        );
        let unmarshal = unmarshal_src(
            "#ifdef VK_KHR_swapchain\n\
             \x20       case VK_STRUCTURE_TYPE_IMAGE_SWAPCHAIN_CREATE_INFO_KHR: {\n\
             \x20           break;\n\
             \x20       }\n\
             #endif\n",
        );

        let gap = compute_gap(&size, &unmarshal);
        assert_eq!(
            gap,
            vec![GapEntry {
                extension: "VK_EXT_extended_dynamic_state3".to_string(),
                structs: vec![
                    "VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_3_FEATURES_EXT"
                        .to_string()
                ],
            }],
            "the handled VK_KHR_swapchain struct must not be reported, the unhandled one must"
        );
    }

    #[test]
    fn gap_ignores_core_version_ifdef_blocks() {
        // A core version is not an extension and cannot be stripped from an enumerate result, so
        // it must never appear in the gap even when the unmarshal switch has no case for it.
        let size = size_src(
            "#ifdef VK_VERSION_1_1\n\
             \x20       case VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SUBGROUP_PROPERTIES: {\n\
             \x20           return sizeof(VkPhysicalDeviceSubgroupProperties);\n\
             \x20       }\n\
             #endif\n\
             #ifdef VK_VERSION_1_3\n\
             \x20       case VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TOOL_PROPERTIES: {\n\
             \x20           return sizeof(VkPhysicalDeviceToolProperties);\n\
             \x20       }\n\
             #endif\n",
        );
        let unmarshal = unmarshal_src("");
        assert!(
            compute_gap(&size, &unmarshal).is_empty(),
            "a VK_VERSION_* block must never yield an extension"
        );
    }

    #[test]
    fn gap_is_empty_when_the_tables_agree() {
        let cases = "#ifdef VK_EXT_memory_budget\n\
             \x20       case VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT: {\n\
             \x20           break;\n\
             \x20       }\n\
             #endif\n";
        assert!(compute_gap(&size_src(cases), &unmarshal_src(cases)).is_empty());
    }

    #[test]
    fn gap_groups_multiple_structs_under_one_extension() {
        let size = size_src(
            "#ifdef VK_KHR_acceleration_structure\n\
             \x20       case VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET_ACCELERATION_STRUCTURE_KHR: {\n\
             \x20           break;\n\
             \x20       }\n\
             \x20       case VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR: {\n\
             \x20           break;\n\
             \x20       }\n\
             \x20       case VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_PROPERTIES_KHR: {\n\
             \x20           break;\n\
             \x20       }\n\
             #endif\n",
        );
        let gap = compute_gap(&size, &unmarshal_src(""));
        assert_eq!(gap.len(), 1, "three cases, one owning extension");
        assert_eq!(gap[0].extension, "VK_KHR_acceleration_structure");
        assert_eq!(
            gap[0].structs,
            vec![
                "VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR",
                "VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_PROPERTIES_KHR",
                "VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET_ACCELERATION_STRUCTURE_KHR",
            ],
            "structs are sorted so the generated .inc is deterministic"
        );
    }

    #[test]
    fn beta_extension_guard_is_not_mistaken_for_the_owning_extension() {
        let size = size_src(
            "#ifdef VK_KHR_video_encode_queue\n\
             #ifdef VK_ENABLE_BETA_EXTENSIONS\n\
             \x20       case VK_STRUCTURE_TYPE_VIDEO_ENCODE_INFO_KHR: {\n\
             \x20           break;\n\
             \x20       }\n\
             #endif  // VK_ENABLE_BETA_EXTENSIONS\n\
             #endif\n",
        );
        let gap = compute_gap(&size, &unmarshal_src(""));
        assert_eq!(gap.len(), 1);
        assert_eq!(gap[0].extension, "VK_KHR_video_encode_queue");
    }

    #[test]
    fn size_table_fn_name_does_not_match_the_stream_features_variant() {
        // Both functions live in one file with identical case sets; picking the wrong one would
        // still "work", which is exactly why the scanner must be pinned to the right name.
        let src = "size_t goldfish_vk_extension_struct_size_with_stream_features(uint32_t f) {\n\
                   \x20   switch (t) {\n\
                   #ifdef VK_EXT_wrong\n\
                   \x20       case VK_STRUCTURE_TYPE_WRONG_EXT: {\n\
                   \x20           break;\n\
                   \x20       }\n\
                   #endif\n\
                   \x20   }\n\
                   }\n\
                   size_t goldfish_vk_extension_struct_size(VkStructureType r) {\n\
                   \x20   switch (t) {\n\
                   #ifdef VK_EXT_right\n\
                   \x20       case VK_STRUCTURE_TYPE_RIGHT_EXT: {\n\
                   \x20           break;\n\
                   \x20       }\n\
                   #endif\n\
                   \x20   }\n\
                   }\n";
        let gap = compute_gap(src, &unmarshal_src(""));
        assert_eq!(gap.len(), 1);
        assert_eq!(gap[0].extension, "VK_EXT_right");
    }

    #[test]
    fn promotion_alias_maps_the_valve_guard_to_its_ext_spelling() {
        // The real case: the codegen files both cases under the VALVE guard but spells them EXT,
        // and the host driver advertises the EXT name. Missing this alias is an abort().
        assert_eq!(
            promotion_alias(
                "VK_VALVE_mutable_descriptor_type",
                &[
                    "VK_STRUCTURE_TYPE_MUTABLE_DESCRIPTOR_TYPE_CREATE_INFO_EXT".to_string(),
                    "VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MUTABLE_DESCRIPTOR_TYPE_FEATURES_EXT"
                        .to_string(),
                ],
            )
            .as_deref(),
            Some("VK_EXT_mutable_descriptor_type")
        );
        // Same vendor tag as the guard: nothing to alias.
        assert_eq!(
            promotion_alias(
                "VK_EXT_memory_budget",
                &["VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT".to_string()],
            ),
            None
        );
        // Mixed tags: the guard collects structs from several extensions, so no alias is derivable.
        assert_eq!(
            promotion_alias(
                "VK_KHR_dynamic_rendering",
                &[
                    "VK_STRUCTURE_TYPE_ATTACHMENT_SAMPLE_COUNT_INFO_AMD".to_string(),
                    "VK_STRUCTURE_TYPE_MULTIVIEW_PER_VIEW_ATTRIBUTES_INFO_NVX".to_string(),
                ],
            ),
            None
        );
    }

    #[test]
    fn rendered_inc_is_deterministic_and_names_the_pin() {
        let gap = vec![GapEntry {
            extension: "VK_VALVE_mutable_descriptor_type".to_string(),
            structs: vec!["VK_STRUCTURE_TYPE_MUTABLE_DESCRIPTOR_TYPE_CREATE_INFO_EXT".to_string()],
        }];
        let a = render_inc(&gap, "deadbeef");
        assert_eq!(
            a,
            render_inc(&gap, "deadbeef"),
            "verify mode depends on this"
        );
        assert!(a.contains("gfxstream pin : deadbeef"));
        assert!(a.contains("DO NOT EDIT"));
        assert!(a.contains("\"VK_VALVE_mutable_descriptor_type\","));
        assert!(
            a.contains("\"VK_EXT_mutable_descriptor_type\","),
            "the promoted spelling must be in the strip list too"
        );
        assert!(a.contains(
            "{\"VK_STRUCTURE_TYPE_MUTABLE_DESCRIPTOR_TYPE_CREATE_INFO_EXT\", \
             \"VK_VALVE_mutable_descriptor_type\"},"
        ));
    }

    /// Cheap, non-hermetic sanity check against the pinned tree, skipped on a fresh clone where
    /// `third_party/gfxstream` has not been bootstrapped yet. The hermetic tests above pin the
    /// parser's semantics; this one pins that those semantics still find the real gap.
    #[test]
    fn the_real_pinned_tree_gap_contains_extended_dynamic_state3() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/gfxstream");
        let (Ok(size), Ok(unmarshal)) = (
            std::fs::read_to_string(root.join(SIZE_TABLE_REL_PATH)),
            std::fs::read_to_string(root.join(UNMARSHAL_REL_PATH)),
        ) else {
            eprintln!(
                "skipping: no gfxstream checkout at {} (run third_party/bootstrap.sh)",
                root.display()
            );
            return;
        };
        let gap = compute_gap(&size, &unmarshal);
        assert!(
            gap.len() > 100,
            "expected a large gap in the pinned tree, got {}",
            gap.len()
        );
        assert!(
            gap.iter()
                .any(|e| e.extension == "VK_EXT_extended_dynamic_state3"),
            "VK_EXT_extended_dynamic_state3 is the latent abort ANGLE would trip; it must be found"
        );
        // The guard whose promoted spelling is what the host driver actually advertises.
        let valve = gap
            .iter()
            .find(|e| e.extension == "VK_VALVE_mutable_descriptor_type")
            .expect("VK_VALVE_mutable_descriptor_type is in the pinned tree's gap");
        assert_eq!(
            promotion_alias(&valve.extension, &valve.structs).as_deref(),
            Some("VK_EXT_mutable_descriptor_type"),
            "without this alias the strip list silently loses today's protection"
        );
    }
}
