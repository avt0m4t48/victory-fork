//! Native D3D12 contract for two FP16 values in one mesh output (fork #283).

use std::fs;
use std::path::Path;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};

use super::host::{Draw, Host};

const SIZE: usize = 16;
const EPSILON: f32 = 0.001;
const PERSPECTIVE_ORACLE: [(usize, usize, f32, f32); 4] = [
    (3, 10, -1.213_768_1, 0.362_318_84),
    (6, 10, 0.425_438_6, -0.614_035_07),
    (3, 5, -0.160_256_42, 0.897_435_9),
    (8, 12, 1.069_672_1, -1.295_082),
];

fn report_warp_capabilities(host: &Host) {
    let mut shader_model = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_6,
    };
    let mut options3 = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
    let mut options7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    // SAFETY: each feature query receives the corresponding writable structure and its size.
    let (adapter, desc) = unsafe {
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_SHADER_MODEL,
                (&raw mut shader_model).cast(),
                size_of_val(&shader_model) as u32,
            )
            .expect("query shader model");
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS3,
                (&raw mut options3).cast(),
                size_of_val(&options3) as u32,
            )
            .expect("query barycentrics");
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut options7).cast(),
                size_of_val(&options7) as u32,
            )
            .expect("query mesh shader tier");
        let factory: IDXGIFactory4 =
            CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).expect("create DXGI factory");
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(host.device.GetAdapterLuid())
            .expect("find D3D12 adapter by its LUID");
        let desc = adapter.GetDesc1().expect("get D3D12 adapter description");
        (
            String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_owned(),
            desc,
        )
    };

    // SAFETY: the module name is NUL-terminated, and the path buffer is writable.
    let name: Vec<_> = "d3d10warp.dll".encode_utf16().chain([0]).collect();
    let mut path = [0u16; 1024];
    let path_len = unsafe {
        let warp = GetModuleHandleW(name.as_ptr());
        assert!(!warp.is_null(), "D3D12 did not load d3d10warp.dll");
        GetModuleFileNameW(warp, path.as_mut_ptr(), path.len() as u32) as usize
    };
    assert!(
        (1..path.len()).contains(&path_len),
        "could not read loaded WARP path"
    );
    let loaded = fs::canonicalize(String::from_utf16_lossy(&path[..path_len]))
        .expect("resolve loaded WARP path");
    let installed = fs::canonicalize(
        std::env::current_exe()
            .expect("find test executable")
            .with_file_name("d3d10warp.dll"),
    )
    .expect("find pinned app-local WARP");
    eprintln!(
        "{adapter} ({:#06x}:{:#06x}), WARP {}, SM {:#x}+, mesh tier {}, barycentrics {}",
        desc.VendorId,
        desc.DeviceId,
        loaded.display(),
        shader_model.HighestShaderModel.0,
        options7.MeshShaderTier.0,
        options3.BarycentricsSupported.as_bool(),
    );
    assert_eq!(loaded, installed, "D3D12 loaded a different WARP DLL");
    assert_ne!(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32, 0);
    assert!(shader_model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0);
    assert_eq!(options7.MeshShaderTier, D3D12_MESH_SHADER_TIER_1);
    assert!(options3.BarycentricsSupported.as_bool());
}

fn bytecode(dir: &Path, label: &str) -> Vec<u8> {
    let path = dir.join(format!("{label}.dxil"));
    fs::read(&path).unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
}

fn assert_pixel(actual: [f32; 4], expected: [f32; 4], label: &str, index: usize) {
    assert!(
        actual.iter().zip(expected).all(|(value, reference)| {
            value.is_finite() && (value - reference).abs() <= EPSILON
        }),
        "{label} pixel ({}, {}): {actual:?}, expected {expected:?}",
        index % SIZE,
        index / SIZE,
    );
}

fn render_pair(
    host: &Host,
    root: &ID3D12RootSignature,
    label: &str,
    ms: &[u8],
    ps: &[u8],
) -> Vec<[f32; 4]> {
    let pso = host
        .mesh_pipeline_with(root, ms, ps)
        .unwrap_or_else(|error| panic!("{label} PSO: {error}"));
    let pixels = host
        .draw(
            root,
            &pso,
            &Draw {
                roots: vec![],
                textures: &[],
                samplers: &[],
                vertices: &[],
                stride: 0,
                size: (SIZE as u32, SIZE as u32),
                mesh_groups: 1,
            },
        )
        .unwrap_or_else(|error| panic!("{label} draw: {error}"));
    assert_eq!(pixels.len(), SIZE * SIZE);
    assert_eq!(pixels[0], [0.0; 4], "{label} outside the triangle");
    pixels
}

#[test]
#[ignore = "requires public DXC fixtures and pinned app-local WARP"]
fn packed_mesh_halves_are_decoded_before_pixel_interpolation() {
    let host = Host::new(true).expect("create an SM 6.6 WARP device");
    report_warp_capabilities(&host);
    let fixture_dir = std::env::var_os("VICTORY_MESH_LINK_DXIL").expect("set fixture directory");
    let fixture_dir = Path::new(&fixture_dir);
    let packed_ms = bytecode(fixture_dir, "1-mesh");
    let split_ms = bytecode(fixture_dir, "0-mesh");
    let packed_ps = bytecode(fixture_dir, "1-pixel");
    let split_ps = bytecode(fixture_dir, "0-pixel");

    let root = host.root_signature_for(&[]).expect("create root signature");
    let packed = render_pair(&host, &root, "packed", &packed_ms, &packed_ps);
    let split = render_pair(&host, &root, "split", &split_ms, &split_ps);
    let packed_flat_ps = bytecode(fixture_dir, "1-pixel_flat");
    let split_flat_ps = bytecode(fixture_dir, "0-pixel_flat");
    let packed_flat = render_pair(&host, &root, "packed flat", &packed_ms, &packed_flat_ps);
    let split_flat = render_pair(&host, &root, "split flat", &split_ms, &split_flat_ps);

    let mut covered = 0;
    for (index, &reference) in split.iter().enumerate() {
        let flat = if reference[3] == 1.0 {
            covered += 1;
            [-2.0, 0.5, 0.0, 1.0]
        } else {
            assert_eq!(reference, [0.0; 4], "unexpected split coverage");
            [0.0; 4]
        };
        assert_eq!(
            packed[index][3], reference[3],
            "packed coverage differs at {index}"
        );
        assert_pixel(packed[index], reference, "packed", index);
        assert_pixel(packed_flat[index], flat, "packed flat", index);
        assert_pixel(split_flat[index], flat, "split flat", index);
    }
    assert!(
        (1..SIZE * SIZE).contains(&covered),
        "missing or full triangle coverage"
    );

    // Independent perspective oracle. The three screen vertices are (2,14), (14,14),
    // and (2,2), with clip-space W = 1, 2, 4. Divide their screen weights by W,
    // renormalize, and apply them to the two exactly representable FP16 values.
    for (x, y, a, b) in PERSPECTIVE_ORACLE {
        let index = y * SIZE + x;
        let expected = [a, b, 0.0, 1.0];
        assert_pixel(packed[index], expected, "packed oracle", index);
        assert_pixel(split[index], expected, "split oracle", index);
    }

    eprintln!("Read back {covered} covered pixels in all four 16x16 mesh/PS combinations");
    assert!(
        host.mesh_pipeline_with(&root, &split_ms, &packed_ps)
            .is_err(),
        "PSO accepted a pixel input whose PAIR semantic is absent from the mesh output"
    );
}

#[test]
#[ignore = "requires synthetic native fixtures and pinned app-local WARP"]
fn fork_emitted_mesh_pairs_match_unpacked_readback() {
    use super::{HostShader, pack_mesh_outputs, translate, validator::Validator};

    let host = Host::new(true).expect("create an SM 6.6 WARP device");
    report_warp_capabilities(&host);
    let dir = std::env::var_os("VICTORY_MESH_LINK_NATIVE").expect("set native fixture directory");
    let dir = Path::new(&dir);
    let mesh = fs::read(dir.join("mesh.cdxil")).unwrap();
    let control = fs::read(dir.join("mesh-control.cdxil")).unwrap();
    let root = host.root_signature_for(&[]).expect("create root signature");
    let validator_path = Validator::default_path().expect("find DXIL validator");
    let mut validator = Validator::load(Some(&validator_path)).expect("load DXIL validator");
    let mut signed = |mut shader: HostShader| {
        validator
            .validate(&mut shader.dxil)
            .expect("validate emitted DXIL");
        shader.dxil
    };
    let split_ms = signed(translate(&control).unwrap());

    for mode in ["flat", "center"] {
        let pixel = fs::read(dir.join(format!("pixel-{mode}.cdxil"))).unwrap();
        assert!(pack_mesh_outputs(&control, &pixel, true).is_none());
        assert_eq!(
            pack_mesh_outputs(&mesh, &pixel, false).is_some(),
            mode == "flat"
        );
        let pair = pack_mesh_outputs(&mesh, &pixel, true).expect("matching pair");
        assert_eq!(
            (pair.mesh.elements(), pair.pixel.elements()),
            (&[16][..], &[0][..])
        );
        let packed_ms = signed(pair.mesh.translate().unwrap());
        let packed_ps = signed(pair.pixel.translate().unwrap());
        let split_ps = signed(translate(&pixel).unwrap());
        let packed = render_pair(&host, &root, "fork packed", &packed_ms, &packed_ps);
        let split = render_pair(&host, &root, "fork split", &split_ms, &split_ps);
        let mut covered = 0;
        for (index, (&actual, &expected)) in packed.iter().zip(&split).enumerate() {
            assert_eq!(actual[3], expected[3], "{mode} coverage at {index}");
            assert_pixel(actual, expected, mode, index);
            if expected[3] == 1.0 {
                covered += 1;
                if mode == "flat" {
                    assert_eq!(actual, [-2.0, 0.5, 0.0, 1.0]);
                }
            }
        }
        assert_eq!(covered, 66, "{mode} coverage");
        if mode == "center" {
            for (x, y, a, b) in PERSPECTIVE_ORACLE {
                let index = y * SIZE + x;
                assert_pixel(packed[index], [a, b, 0.0, 1.0], mode, index);
            }
        }
        eprintln!("Fork-emitted {mode}: read back {covered} matching pixels");
    }
}
