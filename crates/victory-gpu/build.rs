//! The translator's fingerprint, for the shader cache's key (`shader_cache::build`): a hash of
//! what decides a translation and how it is stored (the `victory-shader` and `victory-pipeline`
//! sources, `shader_cache.rs`, the workspace's manifest and lock file, the compiler, the
//! profile). A rebuild that leaves all of it alone keeps the translations cached so far; one that
//! changes any of it never reads them. Keyed by the executable's own hash, as before, every
//! rebuild started cold: some 2,800 translations again, and the first run after it stuttered.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// FNV-1a over 128 bits: two 64-bit lanes from different starts.
struct Fnv(u64, u64);

impl Fnv {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            self.1 = (self.1 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(29);
        }
    }
}

fn main() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let crates = here.parent().expect("the crates folder");
    let root = crates.parent().expect("the workspace root");
    let mut list = Vec::new();
    for name in ["victory-shader", "victory-pipeline"] {
        let dir = crates.join(name);
        files(&dir.join("src"), &mut list);
        list.push(dir.join("Cargo.toml"));
        println!("cargo:rerun-if-changed={}", dir.join("src").display());
        println!("cargo:rerun-if-changed={}", dir.join("Cargo.toml").display());
    }
    for file in [here.join("src").join("shader_cache.rs"), root.join("Cargo.toml"), root.join("Cargo.lock")] {
        println!("cargo:rerun-if-changed={}", file.display());
        list.push(file);
    }
    println!("cargo:rerun-if-changed=build.rs");
    list.sort();
    let mut h = Fnv(0xcbf2_9ce4_8422_2325, 0x6c62_272e_07bb_0142);
    for file in &list {
        let name = file.strip_prefix(root).unwrap_or(file).to_string_lossy().replace('\\', "/");
        h.feed(name.as_bytes());
        h.feed(&[0]);
        let body = std::fs::read(file).unwrap_or_default();
        h.feed(&(body.len() as u64).to_le_bytes());
        h.feed(&body);
    }
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = std::process::Command::new(rustc).arg("-vV").output().map(|o| o.stdout).unwrap_or_default();
    h.feed(&version);
    h.feed(std::env::var("PROFILE").unwrap_or_default().as_bytes());
    // Not the `diagnostics` feature: in the translator it only lets
    // VICTORY_SHADER_LOOP_LIMIT change the loop limit, whose value the key
    // holds anyway (`shader_cache::build`). So a diagnostics build and a
    // plain one share their translations, and a scripted run of the one
    // (which can press through the menus) warms the other's cache.
    println!("cargo:rustc-env=VICTORY_TRANSLATOR_FINGERPRINT={:016x}{:016x}", h.0, h.1);
}
