//! Embeds every `lang/*.json` file in the binary: adding a language means adding a file. Also gives strcu.exe
//! its icon and the version details Windows shows (Properties → Details, Task Manager, Installed apps).

use std::path::Path;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=lang");
    let dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("lang");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("lang directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let mut out = String::from("/// (language code, file contents) for every file in `lang/`.\n");
    out.push_str("pub static FILES: &[(&str, &str)] = &[\n");
    for p in &files {
        let code = p.file_stem().and_then(|s| s.to_str()).expect("file name");
        out.push_str(&format!("    ({code:?}, include_str!({:?})),\n", p.display().to_string()));
    }
    out.push_str("];\n");
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("lang_files.rs"), out).unwrap();
    resources();
}

/// Icon (made from icon/strcu.svg by icon/make.py) and version details; the version is Cargo.toml's.
fn resources() {
    println!("cargo:rerun-if-changed=icon/strcu.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Task Manager lists a process by its description, so it is the name, not a sentence
    let name = if env::var_os("CARGO_FEATURE_DEVBUILD").is_some() { "StrCuDev" } else { "StrCu" };
    winresource::WindowsResource::new()
        .set_icon("icon/strcu.ico")
        .set("ProductName", name)
        .set("FileDescription", name)
        .set("InternalName", "strcu")
        .set("OriginalFilename", "strcu.exe")
        .set("LegalCopyright", "Copyright (c) 2026 Şakir Şek. MIT License.")
        .compile()
        .expect("resource compiler (rc.exe, from the Windows SDK)");
}
