fn main() {
    // Stamp every build so a running session can always be asked which
    // binary it actually is. A daemon that never dies makes "did my fix
    // take effect?" unanswerable otherwise - and this session-skew has
    // now caused two rounds of debugging the wrong process.
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%h %cd", "--date=format:%Y-%m-%d %H:%M"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // Build *time*, not commit time: the question this answers is "is the
    // running session older than the last rebuild", and a commit date says
    // nothing about either.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let stamp = format!("{} built {}", if out.is_empty() { "unknown".to_string() } else { out }, now);
    println!("cargo:rustc-env=MYPIXELS_BUILD={stamp}");
    // Re-run on ANY source change, or the stamp freezes at the last build
    // that touched build.rs and every later binary claims that old time -
    // an odometer that stops is worse than no odometer (observed within
    // the hour of the stamp's introduction, naturally).
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    for entry in std::fs::read_dir("src").expect("src") {
        if let Ok(e) = entry {
            println!("cargo:rerun-if-changed={}", e.path().display());
        }
    }
}
