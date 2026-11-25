#![cfg(any(unix, windows))]

use std::fs;
use std::path::Path;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::symlink as symlink_file;
#[cfg(windows)]
use std::os::windows::fs::symlink_file;

fn run(dir: &Path, path: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_victory"))
        .current_dir(dir)
        .env_remove("VICTORY_CONFIG")
        .env_remove("VICTORY_WARM_SECONDS")
        .env("VICTORY_CACHE_DIR", dir.join("cache"))
        .env("VICTORY_DATA_DIR", dir.join("data"))
        .args(args)
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn shader_commands_accept_symlinked_files() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("original.rpf");
    // A source-built DXBC header is enough to check scanner discovery.
    let mut container = [0; 32];
    container[..4].copy_from_slice(b"DXBC");
    container[20] = 1;
    container[24] = 32;
    fs::write(&target, container).unwrap();

    let game = dir.path().join("linked-game");
    fs::create_dir(&game).unwrap();
    let link = game.join("shaders.rpf");
    if let Err(error) = symlink_file(&target, &link) {
        #[cfg(windows)]
        if error.raw_os_error() == Some(1314) {
            eprintln!("skipping symlink checks because Windows denied symlink creation");
            return;
        }
        panic!("creating the shader symlink failed: {error}");
    }
    symlink_file(dir.path().join("missing.rpf"), game.join("dangling.rpf")).unwrap();

    for (args, expected) in [
        (&["translate"][..], "containers   1 (0 duplicates skipped)"),
        (&["translate", "--warm", "all"], "all hashes asked, 1 found"),
        (&["shader-info"], "containers found       1 (in 1 files)"),
    ] {
        for path in [&target, &link, &game] {
            let stdout = run(dir.path(), path, args);
            assert!(
                stdout.contains(expected),
                "{args:?} {}:\n{stdout}",
                path.display()
            );
            eprintln!("passed {args:?} {}", path.display());
        }
    }
}
