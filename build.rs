fn main() {
    // Прямая линковка с ntdll (NT API вызывается напрямую, без GetProcAddress)
    println!("cargo:rustc-link-lib=ntdll");
    println!("cargo:rerun-if-changed=build.rs");
}
