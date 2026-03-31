use std::path::Path;

fn main() {
    let vsix = Path::new("../../extensions/cursor-budi/cursor-budi.vsix");
    println!("cargo:rerun-if-changed={}", vsix.display());

    if !vsix.exists() {
        std::fs::write(vsix, b"").ok();
    }
}
