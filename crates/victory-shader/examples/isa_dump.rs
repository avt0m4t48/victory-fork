//! Prints a saved console shader container's GFX10 program in SP3 syntax
//! (`VICTORY_SHADER_SAVE`'s `.cdxil` files), with each instruction's byte
//! offset, so a translator error's "GFX10 code at 0x..." can be read in place.
//!
//! `cargo run -p victory-shader --example isa_dump -- <file.cdxil> [from-offset] [count]`
//! (offsets in hex or decimal; without them, the whole program).

use victory_shader::{Container, FourCC};

fn number(s: &str) -> usize {
    match s.strip_prefix("0x") {
        Some(h) => usize::from_str_radix(h, 16).expect("hex offset"),
        None => s.parse().expect("offset"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: isa_dump <file.cdxil> [from-offset] [count]");
        std::process::exit(2);
    };
    let from = args.get(1).map_or(0, |s| number(s));
    let count = args.get(2).map_or(usize::MAX, |s| number(s));
    let bytes = std::fs::read(path).expect("read the container");
    let c = Container::parse(&bytes).expect("a DXBC container");
    let dxil = c.part(FourCC::DXIL).expect("a DXIL part");
    for p in victory_shader::native_programs(dxil.data).expect("native programs") {
        println!(
            "; {:?} program, wave{}, {} VGPRs, {} bytes of code",
            p.stage,
            if p.wave64 { 64 } else { 32 },
            p.vgprs,
            p.code.len()
        );
        let insts = match victory_shader::gfx10::decode(&p.code) {
            Ok(i) => i,
            Err(e) => {
                println!("; does not decode: {e}");
                continue;
            }
        };
        for inst in insts.iter().filter(|i| i.offset >= from).take(count) {
            println!("{:06x}: {}", inst.offset, inst.text(p.wave64));
        }
    }
}
