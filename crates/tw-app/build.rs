#[cfg(target_os = "linux")]
fn main() {
    use std::env;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=build.rs");

    let Some(runtime_library) = find_runtime_library() else {
        return;
    };

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let linker_name = out_dir.join("libxkbcommon-x11.so");
    if !linker_name.exists() {
        if let Err(error) = symlink(&runtime_library, &linker_name) {
            println!(
                "cargo:warning=unable to link {} to {}: {error}",
                linker_name.display(),
                runtime_library.display()
            );
            return;
        }
    }

    // gpui 0.2.2 enables xkbcommon's X11 bindings even for its Wayland backend.
    // Some Linux distributions ship the versioned runtime library without the
    // development symlink that rustc's native linker expects.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
}

#[cfg(target_os = "linux")]
fn find_runtime_library() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    use std::process::Command;

    let output = Command::new("ldconfig").arg("-p").output().ok();
    let listing = output
        .as_ref()
        .map(|output| String::from_utf8_lossy(&output.stdout));
    if let Some(path) = listing.as_deref().and_then(find_library_in_listing) {
        return Some(path);
    }

    [
        "/lib/libxkbcommon-x11.so.0",
        "/lib64/libxkbcommon-x11.so.0",
        "/usr/lib/libxkbcommon-x11.so.0",
        "/usr/lib64/libxkbcommon-x11.so.0",
        "/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0",
        "/usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

#[cfg(target_os = "linux")]
fn find_library_in_listing(listing: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    listing.lines().find_map(|line| {
        let (name, path) = line.split_once("=>")?;
        if !name.contains("libxkbcommon-x11.so") {
            return None;
        }
        let path = PathBuf::from(path.trim());
        path.is_file().then_some(path)
    })
}

#[cfg(not(target_os = "linux"))]
fn main() {}
