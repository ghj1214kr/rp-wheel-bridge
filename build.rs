use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());

    println!("cargo:rustc-link-search={}", out.display());

    let memory_x = include_bytes!("memory.x");

    let mut file = File::create(out.join("memory.x")).unwrap();
    file.write_all(memory_x).unwrap();

    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
}
