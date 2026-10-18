use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(command: &mut Command) {
    let status = command.status().expect("не удалось запустить CMake");
    assert!(status.success(), "ошибка сборки C ABI: {command:?}");
}

fn normalized(path: PathBuf) -> PathBuf {
    let canonical = path.canonicalize().expect("путь C SDK не существует");
    let text = canonical.to_str().expect("CMake требует Unicode-путь");
    // Verbatim-пути Rust (\\?\D:\...) MSBuild превращает в ///?/D:/...,
    // что не является путём исходника. UNC сохраняет свою семантику.
    let text = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()
    };
    PathBuf::from(text.replace('\\', "/"))
}

fn watch(directory: &Path) {
    for item in fs::read_dir(directory).expect("не удалось прочитать исходники C")
    {
        let path = item.expect("ошибка каталога C").path();
        if path.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.starts_with('.') && !name.starts_with("build") && name != "target" {
                watch(&path);
            }
        } else if matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("c" | "h" | "asm" | "cmake" | "in")
        ) || path.file_name().is_some_and(|s| s == "CMakeLists.txt")
        {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn main() {
    for key in [
        "XIPC_SOURCE_DIR",
        "XIPC_SDK_PREFIX",
        "CMAKE",
        "CMAKE_GENERATOR",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-changed=native");
    let target = env::var("TARGET").expect("TARGET");
    let platform = match target.as_str() {
        "x86_64-pc-windows-msvc" => "x64",
        "i686-pc-windows-msvc" => "Win32",
        _ => panic!("xipc-sys поддерживает только Windows MSVC x86/x64, получено {target}"),
    };
    if let Ok(generator) = env::var("CMAKE_GENERATOR") {
        assert!(
            generator.starts_with("Visual Studio "),
            "нужен генератор CMake Visual Studio"
        );
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let cmake = env::var_os("CMAKE").unwrap_or_else(|| "cmake".into());
    // Rust debug тоже использует обычный CRT. C всегда собирается Release,
    // а статический/динамический CRT выбирается по целевым фичам rustc.
    let features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let runtime = if features.split(',').any(|f| f == "crt-static") {
        "MultiThreaded"
    } else {
        "MultiThreadedDLL"
    };
    let source = env::var_os("XIPC_SOURCE_DIR");
    let sdk = env::var_os("XIPC_SDK_PREFIX");
    assert!(
        !(source.is_some() && sdk.is_some()),
        "задайте только XIPC_SOURCE_DIR либо XIPC_SDK_PREFIX"
    );
    let prefix = if let Some(source) = source {
        let source = normalized(PathBuf::from(source));
        watch(&source);
        let syscalls = source.parent().expect("родитель ipc").join("syscalls");
        watch(&syscalls);
        let build = out.join("c-core");
        let prefix = out.join("sdk");
        run(Command::new(&cmake)
            .arg("--fresh")
            .arg("-S")
            .arg(&source)
            .arg("-B")
            .arg(&build)
            .args(["-A", platform, "-DXIPC_CRT_FREE=OFF", "-DXIPC_INSTALL=ON"])
            .arg(format!("-DCMAKE_MSVC_RUNTIME_LIBRARY={runtime}"))
            .arg(format!("-DCMAKE_INSTALL_PREFIX={}", prefix.display())));
        run(Command::new(&cmake)
            .arg("--build")
            .arg(&build)
            .args(["--config", "Release"]));
        run(Command::new(&cmake)
            .arg("--install")
            .arg(&build)
            .args(["--config", "Release"]));
        prefix
    } else {
        normalized(PathBuf::from(sdk.expect(
            "задайте XIPC_SOURCE_DIR=<xhook/ipc> или XIPC_SDK_PREFIX=<установленный hosted SDK>",
        )))
    };
    let header = prefix.join("include/xipc/types.h");
    let archive = prefix.join("lib/xipc.lib");
    watch(&prefix.join("include/xipc"));
    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed={}", archive.display());
    assert!(
        archive.is_file(),
        "нет Release xipc.lib: {}",
        archive.display()
    );
    let text = fs::read_to_string(&header).expect("нет заголовков xipc SDK");
    assert!(
        text.lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>()
                == ["#define", "XIPC_VERSION_MAJOR", "2"]),
        "нужен xipc ABI 2"
    );
    let minor = text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next() == Some("#define") && words.next() == Some("XIPC_VERSION_MINOR") {
            words.next().and_then(|n| n.parse::<u32>().ok())
        } else {
            None
        }
    });
    assert!(
        minor.is_some_and(|n| n >= 2),
        "нужен xipc 2.2+ с объединённым ожиданием активности"
    );
    let probe = out.join("abi-check");
    run(Command::new(&cmake)
        .args(["-S", "native", "-B"])
        .arg(&probe)
        .args(["-A", platform])
        .arg(format!("-DXIPC_ARCHIVE={}", archive.display()))
        .arg(format!("-DXIPC_RUNTIME={runtime}"))
        .arg(format!(
            "-DXIPC_INCLUDE={}",
            prefix.join("include").display()
        )));
    run(Command::new(&cmake)
        .arg("--build")
        .arg(&probe)
        .args(["--config", "Release"]));
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=xipc");
    println!("cargo:rustc-link-lib=kernel32");
    println!("cargo:include={}", prefix.join("include").display());
}
