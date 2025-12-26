use std::path::PathBuf;

fn main() {
    // Линковка системных библиотек для Windows
    println!("cargo:rustc-link-lib=ntdll");
    
    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let header_dir = PathBuf::from(&crate_dir).join("include");
    std::fs::create_dir_all(&header_dir).expect("create include dir");
    let header_path = header_dir.join("xshm.h");

    let config =
        cbindgen::Config::from_file(PathBuf::from(&crate_dir).join("cbindgen.toml")).unwrap();

    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("Unable to generate xshm.h")
        .write_to_file(header_path);
}
