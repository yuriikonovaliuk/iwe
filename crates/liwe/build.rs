// Stamps the iwe-plus version line: this fork's own version, the upstream
// iwe release it is based on (UPSTREAM_VERSION at the workspace root, bumped
// on every upstream merge) and the commit it was built from. IWE_GIT_SHA
// overrides the commit where there is no .git (a Nix build).
use std::process::Command;

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let upstream = std::fs::read_to_string(root.join("UPSTREAM_VERSION"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let sha = std::env::var("IWE_GIT_SHA").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(&root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });
    let version = env!("CARGO_PKG_VERSION");
    println!("cargo:rustc-env=IWE_VERSION_LINE={version} (iwe-plus; upstream iwe {upstream}; {sha})");
    println!("cargo:rerun-if-env-changed=IWE_GIT_SHA");
    println!("cargo:rerun-if-changed={}", root.join("UPSTREAM_VERSION").display());
    println!("cargo:rerun-if-changed={}", root.join(".git/HEAD").display());
    println!("cargo:rerun-if-changed={}", root.join(".git/refs/heads").display());
}
