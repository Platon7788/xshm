fn main() {
    // ntdll импортируется через `#[link(kind = "raw-dylib")]` в
    // `src/ntapi/funcs.rs` -- линковать `ntdll.lib` отсюда не нужно.
    println!("cargo:rerun-if-changed=build.rs");
}
