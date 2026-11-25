//! Matched mesh outputs and pixel inputs that can carry two FP16 values in one uint.

use crate::dxbc::{Container, FourCC, u32_at};
use crate::io::{self, Barycentric, Interp, Target};
use crate::native::{HwStage, NativeProgram, StageIo, Usage};
use crate::{ShaderError, spill};

use super::container::{MESH_OUTPUT_LIMIT, SigElement, mesh_output_bytes};
use super::{HostShader, emit_translated, executable_program, signature};

/// A selected mesh or pixel variant, tied to the input it was planned for.
pub struct PackedStage<'a> {
    container: &'a [u8],
    elements: Vec<usize>,
}

impl PackedStage<'_> {
    /// Original signature element indices, in canonical order for the cache.
    pub fn elements(&self) -> &[usize] {
        &self.elements
    }

    pub fn translate(&self) -> Result<HostShader, ShaderError> {
        let c = Container::parse(self.container)?;
        emit_translated(&c, &executable_program(&c)?, &self.elements)
    }
}

/// The matching translations of a mesh/pixel pair that fits the D3D12 output limit.
pub struct PackedMesh<'a> {
    pub mesh: PackedStage<'a>,
    pub pixel: PackedStage<'a>,
}

struct OversizedMesh<'a> {
    native: NativeProgram,
    raw_outputs: &'a [u8],
    outputs: Vec<SigElement>,
    prims: Vec<SigElement>,
}

/// Whether this mesh is a candidate without examining a pixel shader.
pub fn mesh_output_over_budget(mesh: &[u8]) -> bool {
    oversized_mesh(mesh).is_some()
}

fn oversized_mesh(mesh: &[u8]) -> Option<OversizedMesh<'_>> {
    let mc = Container::parse(mesh).ok()?;
    let native = executable_program(&mc).ok()?;
    let StageIo::Ms(ms) = &native.io else {
        return None;
    };
    if native.stage != HwStage::Ms
        || ms.threads != 128
        || ms.max_vertices != 128
        || ms.topology != 2
    {
        return None;
    }
    let raw_outputs = mc.part(FourCC::OSG1)?.data;
    let outputs = signature(raw_outputs).ok()?;
    let mut prims = match mc.part(FourCC::PSG1) {
        Some(part) => signature(part.data).ok()?,
        None => Vec::new(),
    };
    prims.retain(|e| !matches!(e.kind, 10 | 30));
    if mesh_output_bytes(&outputs, &prims, ms.max_vertices, ms.max_primitives) <= MESH_OUTPUT_LIMIT
    {
        return None;
    }
    Some(OversizedMesh {
        native,
        raw_outputs,
        outputs,
        prims,
    })
}

/// Selects complete FP16 pairs for the 128-vertex meshes whose original
/// signature is over budget. An unproven pairing leaves the normal path in use.
pub fn pack_mesh_outputs<'a>(
    mesh: &'a [u8],
    pixel: &'a [u8],
    barycentrics: bool,
) -> Option<PackedMesh<'a>> {
    let OversizedMesh {
        native: mn,
        raw_outputs,
        mut outputs,
        prims,
    } = oversized_mesh(mesh)?;
    let StageIo::Ms(ms) = &mn.io else {
        return None;
    };
    let fits = |outputs: &[SigElement]| {
        mesh_output_bytes(outputs, &prims, ms.max_vertices, ms.max_primitives) <= MESH_OUTPUT_LIMIT
    };

    let pc = Container::parse(pixel).ok()?;
    let pn = executable_program(&pc).ok()?;
    let StageIo::Ps { inputs: ps, .. } = &pn.io else {
        return None;
    };
    if pn.stage != HwStage::Ps {
        return None;
    }
    let raw_inputs = pc.part(FourCC::ISG1)?.data;
    let inputs = signature(raw_inputs).ok()?;
    let mesh_io = io::raise(&spill::build(&mn).ok()?, &mn).ok()?;
    let pixel_io = io::raise(&spill::build(&pn).ok()?, &pn).ok()?;
    let mut mesh_elements = Vec::new();
    let mut pixel_elements = Vec::new();

    for id in 0..outputs.len() {
        let Some(mr) = pair_row(raw_outputs, &outputs, id) else {
            continue;
        };
        let semantic = &outputs[id];
        if prims.iter().any(|other| same_semantic(semantic, other)) {
            continue;
        }
        let mut native_outputs = ms
            .outputs
            .iter()
            .filter(|entry| entry.row == mr && entry.param & 0x80 == 0);
        if !native_outputs
            .next()
            .is_some_and(|entry| entry.usage == Usage::Generic && entry.fp16 && entry.pack2)
            || native_outputs.next().is_some()
        {
            continue;
        }
        let mut exports = mesh_io.exports.iter().filter(
            |e| matches!(e.target, Target::Param { usage: Usage::Generic, row } if row == mr),
        );
        let has_pair = |e: &io::Export| !e.compressed && e.values[0].is_some();
        if !exports.next().is_some_and(has_pair) || !exports.all(has_pair) {
            continue;
        }

        let Some(ps_id) = inputs
            .iter()
            .position(|other| same_semantic(semantic, other))
        else {
            continue;
        };
        let Some(pr) = pair_row(raw_inputs, &inputs, ps_id) else {
            continue;
        };
        if semantic.name != inputs[ps_id].name {
            continue;
        }
        let mut native_inputs = ps.iter().filter(|entry| entry.row == pr);
        if !native_inputs.next().is_some_and(|entry| {
            entry.usage == Usage::Generic && entry.fp16 && entry.pack2 && entry.row2 == pr
        }) || native_inputs.next().is_some()
            || ps
                .iter()
                .any(|entry| entry.fp16 && entry.pack2 && entry.row != pr && entry.row2 == pr)
            || !supported_reads(&pixel_io, pr, barycentrics)
        {
            continue;
        }

        outputs[id].cols = 1;
        mesh_elements.push(id);
        pixel_elements.push(ps_id);
        if fits(&outputs[..]) {
            pixel_elements.sort_unstable();
            return Some(PackedMesh {
                mesh: PackedStage {
                    container: mesh,
                    elements: mesh_elements,
                },
                pixel: PackedStage {
                    container: pixel,
                    elements: pixel_elements,
                },
            });
        }
    }
    None
}

fn same_semantic(a: &SigElement, b: &SigElement) -> bool {
    a.index == b.index && a.name.eq_ignore_ascii_case(&b.name)
}

// The normalized signature loses sparse component masks. Check the original
// mask and reject ambiguous row or semantic matches before using a pair.
fn pair_row(raw: &[u8], signature: &[SigElement], id: usize) -> Option<u8> {
    let e = &signature[id];
    let row = u8::try_from(e.row?).ok()?;
    if row >= 32 || e.kind != 0 || e.comp != 9 || !e.half || e.cols != 2 {
        return None;
    }
    let base = u32_at(raw, 4).ok()? as usize;
    let mask = *raw.get(base + 32 * id + 24)?;
    if mask != 3 << e.col {
        return None;
    }
    (!signature
        .iter()
        .enumerate()
        .any(|(other_id, other)| other_id != id && (other.row == e.row || same_semantic(e, other))))
    .then_some(row)
}

fn supported_reads(io: &io::Io, row: u8, barycentrics: bool) -> bool {
    let mut flat = None;
    for (_, read) in &io.interps {
        let (attribute, mode) = match read {
            Interp::Vertex {
                attribute,
                vertex: 2,
            } => (attribute, Some(true)),
            Interp::Interpolated {
                attribute,
                barycentric: Barycentric::PerspectiveCenter,
                ..
            } if barycentrics => (attribute, Some(false)),
            Interp::Vertex { attribute, .. }
            | Interp::Interpolated { attribute, .. }
            | Interp::Weighted { attribute } => (attribute, None),
        };
        if attribute.row != row {
            continue;
        }
        let Some(mode) = mode else {
            return false;
        };
        if attribute.channel != 0 || flat.is_some_and(|previous| previous != mode) {
            return false;
        }
        flat = Some(mode);
    }
    flat.is_some()
}
