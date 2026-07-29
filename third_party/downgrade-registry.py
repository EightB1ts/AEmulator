#!/usr/bin/env python3
"""Rewrite a modern vk.xml so the Vulkan-Docs 1.3.269-era registry scripts vendored in
gfxstream (TP_GFXSTREAM_CODEGEN_REF=838dd1d61) can consume it.

USAGE — this is how third_party/gfxstream/codegen/vulkan/vulkan-docs-next/xml/vk.xml was produced,
and the only supported way to reproduce or re-bump it:

    curl -fsSLo /tmp/vk.xml \\
      https://raw.githubusercontent.com/KhronosGroup/Vulkan-Docs/v1.4.348/xml/vk.xml
    python3 third_party/downgrade-registry.py /tmp/vk.xml \\
      third_party/gfxstream/codegen/vulkan/vulkan-docs-next/xml/vk.xml
    bash third_party/regen-cereal.sh --opcode-diff   # HARD GATE, run before anything else

The registry and the vendored C HEADERS must move together: the generated cereal names types by the
registry's spelling, so a bumped vk.xml against stale headers fails to compile on promoted names
(VkRenderingAreaInfo, VkSubresourceLayout2, VkTimeDomainKHR, ...). third_party/gfxstream/common/
vulkan/include/{vulkan,vk_video}/*.h are Vulkan-Headers at the SAME tag. The C++ bindings (*.hpp)
in that directory are deliberately NOT bumped: they are 16 MB, nothing in the host renderer includes
them (only common/end2end/GfxstreamEnd2EndTests.h, which this build does not compile), and carrying
their diff would add ~20 MB to the tracked patch series.

Six incompatibilities, all introduced by upstream registry refactors AFTER 1.3.269:

  (1) funcpointer types.  Since ~1.3.29x <type category="funcpointer"> uses the structured
      <proto>/<param> form.  reg.py:parseTree() looks for a DIRECT <name> child and raises
      RuntimeError("Type without a name!").  Flatten back to the old inline-text form.

  (2) core-version "profile split".  Between v1.4.328 and v1.4.335 the core <feature> blocks were
      split into VK_BASE_VERSION_1_x -> VK_COMPUTE_VERSION_1_x -> VK_GRAPHICS_VERSION_1_x ->
      VK_VERSION_1_x, chained by the feature-level `depends` attribute.  The era reg.py has no
      notion of feature-level `depends`, so VK_VERSION_1_0 arrives with ZERO commands and the
      generator emits no core Vulkan at all (dispatch.py then dies with KeyError: 'global').
      Fold the helper features' <require> blocks back into VK_VERSION_1_x and delete the helpers,
      so the generated code keeps its historical `#ifdef VK_VERSION_1_x` guards.
"""
import re, sys
import xml.etree.ElementTree as ET

src, dst = sys.argv[1], sys.argv[2]
tree = ET.parse(src)
root = tree.getroot()

# ---- (1) funcpointer flattening -------------------------------------------------------------
def flat_text(elem):
    out = [elem.text or '']
    for c in elem:
        out.append((c.text or '') if c.tag == 'name'
                   else '<%s>%s</%s>' % (c.tag, c.text or '', c.tag))
        out.append(c.tail or '')
    return ''.join(out)

nfp = 0
for types in root.findall('types'):
    for te in list(types.findall('type')):
        if te.get('category') != 'funcpointer':
            continue
        proto = te.find('proto')
        if proto is None:
            continue
        name = proto.find('name').text
        ret = [proto.text or '']
        for c in proto:
            if c.tag == 'name':
                break
            ret.append('<%s>%s</%s>%s' % (c.tag, c.text or '', c.tag, c.tail or ''))
        params = ['    ' + ' '.join(flat_text(p).split()) for p in te.findall('param')]
        body = ('\n' + ',\n'.join(params) + ');') if params else 'void);'
        attrs = ''.join(' %s="%s"' % (k, v) for k, v in te.attrib.items() if k != 'category')
        new = ET.fromstring('<type category="funcpointer"%s>typedef %s (VKAPI_PTR *<name>%s</name>)(%s</type>'
                            % (attrs, ''.join(ret).strip(), name, body))
        new.tail = te.tail
        i = list(types).index(te)
        types.remove(te)
        types.insert(i, new)
        nfp += 1

# ---- (2) collapse the core-version profile split --------------------------------------------
feats = {f.get('name'): f for f in root.findall('feature')}
HELPERS = ('VK_BASE_VERSION_%s', 'VK_COMPUTE_VERSION_%s', 'VK_GRAPHICS_VERSION_%s')
ncollapsed = 0
for ver in ('1_0', '1_1', '1_2', '1_3', '1_4'):
    target = feats.get('VK_VERSION_%s' % ver)
    if target is None:
        continue
    merged = []
    for tmpl in HELPERS:
        h = feats.get(tmpl % ver)
        if h is None:
            continue
        merged.extend(list(h.findall('require')) + list(h.findall('remove')))
        root.remove(h)
        ncollapsed += 1
    if not merged:
        continue
    own = list(target)
    for e in own:
        target.remove(e)
    for e in merged + own:
        target.append(e)
    if target.get('depends'):
        del target.attrib['depends']


# ---- (3) api="..." attribute lists ------------------------------------------------------------
# cereal/common/vulkantypes.py tests `not "vulkan" == member.attrib["api"]` — an EXACT string
# compare.  Since the profile split, core members carry api="vulkan,vulkanbase", so every such
# member is silently DROPPED from the struct model: the generator then emits the Python `len`
# name (e.g. `initialDataSize`, `colorAttachmentCount`, `pStages`) as a bare C++ identifier.
# Collapse any api list that contains "vulkan" down to exactly "vulkan".
napi = 0
for el in root.iter():
    a = el.get('api')
    if a and a != 'vulkan' and 'vulkan' in a.split(','):
        el.set('api', 'vulkan')
        napi += 1

# ---- (4) <member> that lost its len= ----------------------------------------------------------
# VkDeviceCreateInfo::ppEnabledLayerNames is deprecated="unused" since ~1.4.33x and upstream
# dropped its len attribute; the generator then emits `saveStringArray(..., None)`.  The C struct
# still has the field and gfxstream still marshals it, so put the length back.
LEN_FIXUPS = {('VkDeviceCreateInfo', 'ppEnabledLayerNames'): 'enabledLayerCount,null-terminated'}
nlen = 0
for te in root.findall('types/type'):
    tn = te.get('name')
    for m in te.findall('member'):
        nm = m.find('name')
        if nm is None:
            continue
        key = (tn, nm.text)
        if key in LEN_FIXUPS and m.get('len') is None:
            m.set('len', LEN_FIXUPS[key])
            nlen += 1


# ---- (5) sortorder / type-ownership theft ------------------------------------------------------
# generator.py:regSortFeatures() sorts LAST by the extension's `sortorder`, and reg.py declares each
# type exactly once, under the FIRST feature that requires it.  VK_KHR_ray_tracing_pipeline carries
# sortorder="1" (Khronos deliberately emits it last).  That was harmless while it was the only
# consumer of VkStridedDeviceAddressRegionKHR; 1.4.348 adds VK_NV_cluster_acceleration_structure,
# which also requires that struct, is NOT in SUPPORTED_FEATURES (it has bit-field members the era
# codegen cannot marshal at all), and now declares it first — so the marshal/unmarshal bodies are
# never emitted and VkDecoder.cpp fails to compile.  Push the thief behind the sortorder="1" group
# rather than pulling the KHR extensions forward, which would reorder ~4000 lines of output.
LATE_SORT = {'VK_NV_cluster_acceleration_structure': '2'}
nsort = 0
for e in root.findall('extensions/extension'):
    v = LATE_SORT.get(e.get('name'))
    if v is not None:
        e.set('sortorder', v)
        nsort += 1

# ---- (6) reciprocal <require> inversion --------------------------------------------------------
# reg.py declares each command/type EXACTLY ONCE, under the FIRST feature that <require>s it, and
# generator.py:regSortFeatures() orders extensions by NUMBER. Upstream used to hang the
# "vendor extension X interacting with VK_KHR_synchronization2" declarations off SYNCHRONIZATION2
# (extension 315); since ~1.3.28x it hangs them off the VENDOR extension instead:
#     VK_AMD_buffer_marker                 (180)  vkCmdWriteBufferMarker2AMD
#     VK_NV_device_diagnostic_checkpoints  (207)  vkGetQueueCheckpointData2NV + its two structs
# 180 and 207 both sort BEFORE 315, so those commands are now declared under a feature that is not
# in SUPPORTED_FEATURES, and their marshalling — and their OP_* opcodes — simply vanish. A DELETED
# opcode is a wire break against the PREBUILT guest ICD, which is what regen-cereal.sh
# --opcode-diff exists to catch. Move the blocks back where the 1.3.269-era registry had them.
RECIPROCAL = {'VK_AMD_buffer_marker': 'VK_KHR_synchronization2',
              'VK_NV_device_diagnostic_checkpoints': 'VK_KHR_synchronization2'}
exts = {e.get('name'): e for e in root.findall('extensions/extension')}
nrecip = 0
for donor_name, host_name in RECIPROCAL.items():
    donor, host = exts.get(donor_name), exts.get(host_name)
    if donor is None or host is None:
        continue
    for req in list(donor.findall('require')):
        dep = req.get('depends') or ''
        if host_name not in dep.split(','):
            continue
        donor.remove(req)
        req.set('depends', donor_name)
        host.append(req)
        nrecip += 1

tree.write(dst, encoding='UTF-8', xml_declaration=True)
print('funcpointers flattened: %d | helper features collapsed: %d | api lists normalised: %d | '
      'len fixups: %d | sortorder pushed: %d | reciprocal requires restored: %d -> %s'
      % (nfp, ncollapsed, napi, nlen, nsort, nrecip, dst))
