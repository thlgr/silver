// Release builds embed apps/web/dist, so build the UI first; a stale UI in a release binary is
// easy to ship by accident. Debug builds serve dist from disk, so they skip this and stay fast.
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let web = Path::new("../web");
    for input in [
        "src",
        "index.html",
        "package.json",
        "package-lock.json",
        "vite.config.js",
    ] {
        println!("cargo:rerun-if-changed=../web/{input}");
    }
    // The Docker Rust stage has no Node and gets dist from its own web stage.
    if !web.join("package.json").exists() || Command::new("npm").arg("--version").output().is_err()
    {
        println!("cargo:warning=npm or apps/web not found; embedding the existing apps/web/dist");
        return;
    }
    if !web.join("node_modules").exists() {
        npm(web, &["ci"]);
    }
    npm(web, &["run", "build"]);
}

fn npm(dir: &Path, args: &[&str]) {
    let status = Command::new("npm").args(args).current_dir(dir).status();
    if !matches!(status, Ok(status) if status.success()) {
        panic!(
            "`npm {}` failed in apps/web; fix the UI build above",
            args.join(" ")
        );
    }
}
