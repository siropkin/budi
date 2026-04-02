use std::fs;
use std::path::Path;

fn main() {
    let dist = Path::new("static/dashboard-v2-dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    emit_rerun_for_dir(dist);
}

fn emit_rerun_for_dir(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            emit_rerun_for_dir(&path);
            continue;
        }
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
