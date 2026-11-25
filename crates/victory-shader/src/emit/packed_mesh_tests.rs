use crate::native::{MeshIo, PsInput, StageIo, Usage, VsOutput};
use crate::{Container, FourCC, HwStage, NativeProgram, io, resource, spill, structure};

use super::container::SigElement;
use super::lower::{self, Parts};

fn element(name: &str, row: u32, kind: u8, cols: u8) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col: 0,
        cols,
        usage: 0,
        half: false,
    }
}

fn lower_fixture(
    native: &NativeProgram,
    inputs: Vec<SigElement>,
    outputs: Vec<SigElement>,
    packed: &[usize],
) -> Vec<u8> {
    let p = spill::build(native).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, native).unwrap();
    lower::lower_with_packed_varyings(
        &Parts {
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs,
            outputs,
            prims: Vec::new(),
            mesh_stride: 128,
        },
        packed,
    )
    .unwrap()
    .dxil
}

fn mesh_fixture() -> (NativeProgram, Vec<SigElement>) {
    let mut words = vec![
        0x7e14_02ff,
        0.5_f32.to_bits(), // v_mov_b32 v10, 0.5
        0x7e16_02ff,
        1.0_f32.to_bits(), // v_mov_b32 v11, 1.0
        0x7e18_02ff,
        0xc000_3c00, // v_mov_b32 v12, packed (half 1.0, half -2.0)
        0x7e1a_02ff,
        125 | (126 << 10) | (127 << 20),
        0xf800_00cf,
        0x0b0a_0a0a, // exp pos0, v10, v10, v10, v11
    ];
    let mut parameters = Vec::new();
    let mut outputs = vec![element("SV_Position", 0, 3, 4)];
    for param in 0..16_u8 {
        let pair = param == 15;
        let (cols, mask) = match param {
            14 => (3, 7),
            15 => (2, 1),
            _ => (4, 15),
        };
        let target = 32 + u32::from(param);
        words.extend([
            0xf800_0000 | (target << 4) | mask,
            if pair { 12 } else { 0x0a0a_0a0a },
        ]);
        let mut row = element(
            if pair { "PAIR" } else { "FILLER" },
            u32::from(param + 1),
            0,
            cols,
        );
        row.index = u32::from(param);
        row.half = pair;
        outputs.push(row);
        parameters.push(VsOutput {
            usage: Usage::Generic,
            row: param + 1,
            param,
            fp16: pair,
            pack2: pair,
        });
    }
    words.extend([0xf800_0941, 13, 0xbf81_0000]); // exp prim; s_endpgm
    let native = NativeProgram {
        stage: HwStage::Ms,
        wave64: true,
        vgprs: 32,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Ms(MeshIo {
            outputs: parameters,
            clip_cntl: 0,
            threads: 128,
            max_vertices: 128,
            max_primitives: 1,
            topology: 2,
            lds_bytes: 0,
        }),
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    };
    (native, outputs)
}

#[test]
fn a_native_half_pair_occupies_one_mesh_output_scalar() {
    let (native, outputs) = mesh_fixture();
    let pair = outputs.iter().position(|e| e.name == "PAIR").unwrap();
    let bytes = lower_fixture(&native, Vec::new(), outputs, &[pair]);
    let dxil = Container::parse(&bytes).unwrap();
    let psv = dxil.part(FourCC::PSV0).unwrap().data;
    assert_eq!(u16::from_le_bytes(psv[16..18].try_into().unwrap()), 128);
    let outputs = super::signature(dxil.part(FourCC::OSG1).unwrap().data).unwrap();
    assert_eq!(outputs.iter().map(|e| u32::from(e.cols)).sum::<u32>(), 64);
    let packed = outputs.iter().find(|e| e.name == "PAIR").unwrap();
    assert_eq!((packed.comp, packed.cols), (5, 1));
}

#[test]
fn a_native_flat_half_pair_reads_one_pixel_input_scalar() {
    let words = [
        0xc80e_0002_u32, // v_interp_mov_f32 v3, p0, attr0.x
        0xf800_080f,
        0x0303_0303, // exp mrt0, v3, v3, v3, v3 done
        0xbf81_0000,
    ];
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
                row: 3,
                mask: 3,
                attr: 0,
                flat: true,
                fp16: true,
                pack2: true,
                row2: 3,
            }],
            input_addr: 0,
            db_shader_control: 0,
            patches: Vec::new(),
        },
        code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    };
    let mut input = element("PAIR", 3, 0, 2);
    input.index = 15;
    input.half = true;
    let bytes = lower_fixture(
        &native,
        vec![input],
        vec![element("SV_Target", 0, 16, 4)],
        &[0],
    );
    let dxil = Container::parse(&bytes).unwrap();
    let inputs = super::signature(dxil.part(FourCC::ISG1).unwrap().data).unwrap();
    assert_eq!((inputs[0].comp, inputs[0].cols), (5, 1));
}
