//! Embeds every `lang/*.json` file in the binary: adding a language means adding a file.

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
}
