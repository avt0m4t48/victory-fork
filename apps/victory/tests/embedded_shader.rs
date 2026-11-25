use std::fs;
use std::path::Path;
use std::process::Command;

fn container(seed: u8) -> [u8; 32] {
    // A minimal synthetic DXBC header with a distinct digest. The test
    // checks discovery by hash and needs no shader program in the header.
    let mut bytes = [0; 32];
    bytes[..4].copy_from_slice(b"DXBC");
    bytes[4] = seed;
    bytes[20] = 1;
    bytes[24] = 32;
    bytes
}

fn warm(dir: &Path, pack: &Path, wanted: &Path) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_victory"))
        .current_dir(dir)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_WARM_SECONDS")
        .env("VICTORY_CACHE_DIR", dir.join("cache"))
        .env("VICTORY_DATA_DIR", dir.join("data"))
        .arg("translate")
        .arg(pack)
        .arg("--warm")
        .arg(wanted)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

#[test]
fn warm_finds_each_embedded_shader_by_its_standalone_hash() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("wrapped.rpf");
    let wanted = dir.path().join("wanted.txt");
    let first = container(1);
    let second = container(2);
    fs::write(
        &pack,
        [b"prefix".as_slice(), &first, &second, b"suffix"].concat(),
    )
    .unwrap();

    for shader in [first, second] {
        fs::write(&wanted, &blake3::hash(&shader).to_hex()[..16]).unwrap();
        let summary = warm(dir.path(), &pack, &wanted);
        assert!(summary.contains("1 hashes asked, 1 found"), "{summary}");
    }
}
