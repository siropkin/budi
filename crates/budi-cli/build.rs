use std::path::Path;

fn main() {
    let vsix = Path::new("../../extensions/cursor-budi/cursor-budi.vsix");
    println!("cargo:rerun-if-changed={}", vsix.display());

    let require_vsix = std::env::var("BUDI_REQUIRE_CURSOR_VSIX")
        .map(|v| v == "1")
        .unwrap_or(false);

    let vsix_size = std::fs::metadata(vsix).map(|m| m.len()).unwrap_or(0);

    if require_vsix && vsix_size == 0 {
        panic!(
            "Cursor extension package is missing or empty at {}. \
             Build it before compiling (`npm ci && npm run build && npx vsce package ...`).",
            vsix.display()
        );
    }

    if vsix_size == 0 {
        println!(
            "cargo:warning=Cursor extension package not found at {}. \
             Auto-install in `budi init` will be disabled for this build.",
            vsix.display()
        );
        if !vsix.exists() {
            let _ = std::fs::write(vsix, b"");
        }
    }
}
