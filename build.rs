fn main() {
    println!("cargo:rerun-if-changed=Bandage/ogdf");
    println!("cargo:rerun-if-changed=Bandage/Bandage.pro");
    println!("cargo:rerun-if-changed=native");
    let project = std::fs::read_to_string("Bandage/Bandage.pro")
        .expect("The bundled Bandage source directory is required for its FMMM layout engine");
    let sources: Vec<_> = project
        .lines()
        .map(|line| line.trim().trim_end_matches('\\').trim())
        .filter(|line| line.starts_with("ogdf/") && line.ends_with(".cpp"))
        .map(|line| format!("Bandage/{line}"))
        .collect();
    assert!(!sources.is_empty(), "Bandage OGDF source list is empty");
    let mut native = cc::Build::new();
    native
        .cpp(true)
        .std("c++14")
        .opt_level(3)
        .debug(false)
        .warnings(false)
        .include("Bandage")
        .include("Bandage/ogdf")
        .include("native/qt_geometry")
        .file("native/bandage_layout.cpp")
        .files(sources);

    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    #[cfg(unix)]
    if target_env == "msvc" {
        // cargo-xwin configures clang-cl and AR=llvm-lib, but a standard Rust
        // installation does not put an `llvm-lib` executable on PATH. Rust's
        // bundled lld-link implements the same librarian interface when cc-rs
        // passes its /lib-style arguments, so use it directly.
        let rustc = std::env::var_os("RUSTC").expect("Cargo did not set RUSTC");
        let sysroot = std::process::Command::new(rustc)
            .args(["--print", "sysroot"])
            .output()
            .expect("failed to query the Rust sysroot");
        assert!(sysroot.status.success(), "rustc --print sysroot failed");
        let host = std::env::var("HOST").expect("Cargo did not set HOST");
        let lld_link = std::path::PathBuf::from(
            String::from_utf8(sysroot.stdout)
                .expect("Rust sysroot is not UTF-8")
                .trim(),
        )
        .join("lib/rustlib")
        .join(host)
        .join("bin/gcc-ld/lld-link");
        assert!(
            lld_link.is_file(),
            "Rust's bundled lld-link was not found at {}",
            lld_link.display()
        );
        // lld selects its driver mode from argv[0]. The bundled executable is
        // named `lld-link`, so wrap it and explicitly select librarian mode.
        let wrapper = std::path::PathBuf::from(
            std::env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"),
        )
        .join("llvm-lib");
        let escaped_lld = lld_link.to_string_lossy().replace('\'', "'\\''");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec '{escaped_lld}' /lib \"$@\"\n"),
        )
        .expect("failed to write the llvm-lib wrapper");
        std::fs::set_permissions(
            &wrapper,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("failed to make the llvm-lib wrapper executable");
        native.archiver(wrapper);
    }

    native.compile("bandage_layout");
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=psapi");
    }
}
