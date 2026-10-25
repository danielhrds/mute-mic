use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("app.o");
    let status = Command::new("llvm-windres")
        .current_dir(root)
        .args(["-i", "assets/app.rc", "-O", "coff", "-o"])
        .arg(&output)
        .status()
        .expect("LLVM-MinGW llvm-windres must be available on PATH; use build.ps1");
    assert!(status.success(), "Windows resource compilation failed");
    println!("cargo:rustc-link-arg-bin=mutemic={}", output.display());
    println!("cargo:rustc-link-arg-bin=ui-check={}", output.display());
}
