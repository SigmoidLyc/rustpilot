fn main() {
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=RUSTPILOT_WINDRES");
    println!("cargo:rerun-if-env-changed=RUSTPILOT_WEBVIEW2_LOADER");
    println!("cargo:rerun-if-changed=resources/WebView2Loader.dll");
    println!("cargo:rerun-if-changed=resources/rustpilot.exe.manifest");
    copy_webview_loader();
    copy_external_manifest();
    let skip_requested = std::env::var_os("RUSTPILOT_SKIP_TAURI_BUILD").is_some();
    let windres = std::env::var_os("RUSTPILOT_WINDRES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("windres.exe"));
    let gnu_without_windres = std::env::var("CARGO_CFG_TARGET_ENV")
        .map(|environment| environment == "gnu")
        .unwrap_or(false)
        && std::process::Command::new(windres)
            .arg("--version")
            .output()
            .is_err();
    if skip_requested || gnu_without_windres {
        println!("cargo:warning=Skipping optional Windows exe resource generation because windres is unavailable");
        return;
    }
    tauri_build::build();
}

fn copy_webview_loader() {
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").ok().as_deref() != Some("gnu")
        || std::env::var("CARGO_CFG_TARGET_ARCH").ok().as_deref() != Some("x86_64")
    {
        return;
    }

    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"),
    );
    let bundled = manifest_dir.join("resources").join("WebView2Loader.dll");
    let source = std::env::var_os("RUSTPILOT_WEBVIEW2_LOADER")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or(bundled);
    if !source.is_file() {
        println!(
            "cargo:warning=No compatible WebView2Loader.dll found at {}",
            source.display()
        );
        return;
    }

    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .map(std::path::Path::to_path_buf)
        .expect("OUT_DIR is inside a Cargo profile directory");
    let destination = profile_dir.join("WebView2Loader.dll");
    let source_bytes = match std::fs::read(&source) {
        Ok(bytes) => bytes,
        Err(error) => {
            println!(
                "cargo:warning=Unable to read WebView2Loader.dll from {}: {error}",
                source.display()
            );
            return;
        }
    };
    let destination_matches = std::fs::read(&destination)
        .map(|bytes| bytes == source_bytes)
        .unwrap_or(false);
    if !destination_matches {
        if let Err(error) = std::fs::copy(&source, &destination) {
            println!(
                "cargo:warning=Unable to copy WebView2Loader.dll to {}: {error}",
                destination.display()
            );
            return;
        }
    }
    let verified = std::fs::read(&destination)
        .map(|bytes| bytes == source_bytes)
        .unwrap_or(false);
    if verified {
        println!(
            "cargo:warning=Using RustPilot WebView2 loader {} -> {}",
            source.display(),
            destination.display()
        );
    } else {
        println!(
            "cargo:warning=WebView2Loader.dll verification failed at {}",
            destination.display()
        );
    }
}

fn copy_external_manifest() {
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows") {
        return;
    }

    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"),
    );
    let source = manifest_dir
        .join("resources")
        .join("rustpilot.exe.manifest");
    if !source.is_file() {
        println!(
            "cargo:warning=No RustPilot application manifest found at {}",
            source.display()
        );
        return;
    }

    let mut profile_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    profile_dir.pop();
    profile_dir.pop();
    profile_dir.pop();
    let destination = profile_dir.join("rustpilot.exe.manifest");
    match std::fs::copy(&source, &destination) {
        Ok(_) => println!(
            "cargo:warning=Using external RustPilot application manifest {}",
            destination.display()
        ),
        Err(error) => println!(
            "cargo:warning=Unable to copy RustPilot application manifest to {}: {error}",
            destination.display()
        ),
    }
}
