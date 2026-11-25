use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn embedded_warm_lookup_populates_only_the_requested_exact_container() {
    let fixtures = PathBuf::from(std::env::var_os("VICTORY_EMBEDDED_FIXTURES").unwrap());
    let first = fs::read(fixtures.join("deep.dxbc")).unwrap();
    let second = fs::read(fixtures.join("approx-only.dxbc")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("wrapped.rpf");
    let wanted = dir.path().join("wanted.txt");
    let cache = dir.path().join("cache");
    fs::write(&pack, [b"padding".as_slice(), &first, &second, b"end"].concat()).unwrap();
    fs::write(&wanted, &blake3::hash(&first).to_hex()[..16]).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_victory"))
        .current_dir(dir.path())
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_WARM_SECONDS")
        .env("VICTORY_CACHE_DIR", &cache)
        .env("VICTORY_DATA_DIR", dir.path().join("data"))
        .args(["translate", "--warm"])
        .arg(&wanted)
        .arg(&pack)
        .output()
        .unwrap();
    let out = String::from_utf8(result.stdout).unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(out.contains("1 hashes asked, 1 found: 1 translated now"), "{out}");

    victory_shader::emit::set_host_max_wave(32);
    victory_hle::modules::d3d12x::use_shader_cache(cache.join("shaders"));
    assert_eq!(victory_hle::modules::d3d12x::warm_shader(&first).unwrap(), false);
    assert_eq!(victory_hle::modules::d3d12x::warm_shader(&second).unwrap(), true);
    assert_eq!(victory_hle::modules::d3d12x::warm_shader(&second).unwrap(), false);
}
