fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let cargo_version = package_version(&manifest_dir.join("Cargo.toml"));
    let installer_version = json_version(&manifest_dir.join("tauri.conf.json"));
    let npm_version = json_version(&manifest_dir.join("../package.json"));

    if installer_version != cargo_version || npm_version != cargo_version {
        panic!(
            "app version drift: tauri.conf.json={installer_version} Cargo.toml={cargo_version} package.json={npm_version}"
        );
    }

    assert_cast_ports(&manifest_dir);
    fetch_ffmpeg(&manifest_dir);

    tauri_build::build()
}

/// Download the pinned full ffmpeg (libx264) on Windows and refuse a hash mismatch.
/// Other hosts only need the resource paths to exist so `tauri_build` can read them.
fn fetch_ffmpeg(manifest_dir: &std::path::Path) {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target != "windows" {
        ensure_ffmpeg_placeholder(manifest_dir);
        return;
    }
    fetch_ffmpeg_windows(manifest_dir);
}

fn ensure_ffmpeg_placeholder(manifest_dir: &std::path::Path) {
    let dir = manifest_dir.join("resources").join("ffmpeg");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create ffmpeg resource dir: {e}"));
    let exe = dir.join("ffmpeg.exe");
    let license = dir.join("LICENSE");
    if !exe.is_file() {
        std::fs::write(&exe, b"placeholder\n")
            .unwrap_or_else(|e| panic!("write ffmpeg placeholder: {e}"));
    }
    if !license.is_file() {
        std::fs::write(
            &license,
            "Placeholder. The Windows build replaces this with the pinned ffmpeg license.\n",
        )
        .unwrap_or_else(|e| panic!("write ffmpeg license placeholder: {e}"));
    }
}

#[cfg(not(windows))]
fn fetch_ffmpeg_windows(_manifest_dir: &std::path::Path) {
    panic!("the pinned Windows ffmpeg is verified on a Windows host");
}

#[cfg(windows)]
fn fetch_ffmpeg_windows(manifest_dir: &std::path::Path) {
    use std::process::Command;

    let pin_text = std::fs::read_to_string(manifest_dir.join("ffmpeg.pin.json"))
        .unwrap_or_else(|e| panic!("read ffmpeg.pin.json: {e}"));
    let version = json_field(&pin_text, "version");
    let url = json_field(&pin_text, "url");
    let archive_sha = json_field(&pin_text, "archive_sha256");
    let exe_sha = json_field(&pin_text, "exe_sha256");
    let exe_inside = json_field(&pin_text, "exe_inside");
    let license_inside = json_field(&pin_text, "license_inside");

    let dest_dir = manifest_dir.join("resources").join("ffmpeg");
    std::fs::create_dir_all(&dest_dir).unwrap_or_else(|e| panic!("create ffmpeg resource dir: {e}"));
    let exe_dest = dest_dir.join("ffmpeg.exe");
    let license_dest = dest_dir.join("LICENSE");
    if exe_dest.is_file()
        && license_dest.is_file()
        && sha256_file(&exe_dest) == exe_sha.to_ascii_lowercase()
    {
        assert_libx264(&exe_dest, &version);
        copy_beside_profile(&exe_dest);
        return;
    }

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let archive = out_dir.join(format!("ffmpeg-{version}-full_build.7z"));
    let status = Command::new("curl.exe")
        .args(["-fsSL", "--retry", "3", "--retry-delay", "2", "-o"])
        .arg(&archive)
        .arg(&url)
        .status()
        .unwrap_or_else(|e| panic!("curl pinned ffmpeg: {e}"));
    if !status.success() {
        panic!("curl pinned ffmpeg exited {status}");
    }
    let got = sha256_file(&archive);
    if got != archive_sha.to_ascii_lowercase() {
        panic!("ffmpeg pin mismatch for {version}: expected {archive_sha} got {got}");
    }

    let seven = std::path::PathBuf::from(r"C:\Program Files\7-Zip\7z.exe");
    if !seven.is_file() {
        panic!("7-Zip is required to unpack pinned ffmpeg");
    }
    let extract = out_dir.join(format!("ffmpeg-{version}"));
    let _ = std::fs::remove_dir_all(&extract);
    std::fs::create_dir_all(&extract).unwrap_or_else(|e| panic!("create ffmpeg extract dir: {e}"));
    let status = Command::new(&seven)
        .arg("e")
        .arg("-y")
        .arg(format!("-o{}", extract.display()))
        .arg(&archive)
        .arg(&exe_inside)
        .arg(&license_inside)
        .status()
        .unwrap_or_else(|e| panic!("7z pinned ffmpeg: {e}"));
    if !status.success() {
        panic!("7z pinned ffmpeg exited {status}");
    }
    let extracted_exe = extract.join("ffmpeg.exe");
    let extracted_license = extract.join("LICENSE");
    if !extracted_exe.is_file() || !extracted_license.is_file() {
        panic!("pinned ffmpeg archive did not contain ffmpeg.exe and LICENSE");
    }
    let got = sha256_file(&extracted_exe);
    if got != exe_sha.to_ascii_lowercase() {
        panic!("ffmpeg pin mismatch for {version}: expected {exe_sha} got {got}");
    }
    std::fs::copy(&extracted_exe, &exe_dest).unwrap_or_else(|e| panic!("install ffmpeg.exe: {e}"));
    std::fs::copy(&extracted_license, &license_dest)
        .unwrap_or_else(|e| panic!("install ffmpeg LICENSE: {e}"));
    assert_libx264(&exe_dest, &version);
    copy_beside_profile(&exe_dest);
}

#[cfg(windows)]
fn assert_libx264(exe: &std::path::Path, version: &str) {
    let out = std::process::Command::new(exe)
        .arg("-encoders")
        .output()
        .unwrap_or_else(|e| panic!("ffmpeg -encoders: {e}"));
    let text = String::from_utf8_lossy(&out.stdout);
    for name in ["libx264", "h264_mf", "h264_nvenc", "h264_qsv"] {
        if !text.contains(name) {
            panic!("pinned ffmpeg {version} is missing encoder {name}");
        }
    }
}

#[cfg(windows)]
fn copy_beside_profile(exe: &std::path::Path) {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(profile) = out_dir.ancestors().nth(3) else {
        return;
    };
    let dest = profile.join("ffmpeg.exe");
    if dest.is_file() && sha256_file(&dest) == sha256_file(exe) {
        return;
    }
    std::fs::copy(exe, &dest).unwrap_or_else(|e| panic!("copy ffmpeg beside the app: {e}"));
}

#[cfg(windows)]
fn sha256_file(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(windows)]
fn json_field(text: &str, key: &str) -> String {
    let needle = format!("\"{key}\"");
    for line in text.lines() {
        let line = line.trim().trim_end_matches(',');
        let Some(rest) = line.strip_prefix(&needle) else {
            continue;
        };
        let rest = rest.trim().trim_start_matches(':').trim();
        let value = rest.trim_matches('"');
        if !value.is_empty() {
            return value.to_string();
        }
    }
    panic!("ffmpeg.pin.json is missing {key}");
}

/// `CAST_FW_TCP` in the NSIS template must match `CAST_PORT_LO`/`CAST_PORT_HI`.
fn assert_cast_ports(manifest_dir: &std::path::Path) {
    let cast = std::fs::read_to_string(manifest_dir.join("src/cast.rs"))
        .unwrap_or_else(|e| panic!("read cast.rs: {e}"));
    let lo = rust_u16_const(&cast, "CAST_PORT_LO");
    let hi = rust_u16_const(&cast, "CAST_PORT_HI");
    let nsi = std::fs::read_to_string(manifest_dir.join("windows/installer.nsi"))
        .unwrap_or_else(|e| panic!("read installer.nsi: {e}"));
    let expected = format!("!define CAST_FW_TCP \"{lo}-{hi}\"");
    if !nsi.lines().any(|line| line.trim() == expected) {
        panic!("cast port range drift: installer.nsi must contain {expected}");
    }
}

fn rust_u16_const(src: &str, name: &str) -> u16 {
    let prefix = format!("const {name}: u16 = ");
    for line in src.lines() {
        let trimmed = line.trim().trim_start_matches("pub ").trim();
        let Some(rest) = trimmed.strip_prefix(&prefix) else {
            continue;
        };
        let num = rest.trim().trim_end_matches(';').trim();
        return num
            .parse()
            .unwrap_or_else(|_| panic!("bad {name} in cast.rs"));
    }
    panic!("{name} not found in cast.rs");
}

/// First `"version"` string in a JSON file. For this repo that is the app version.
fn json_version(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("\"version\"") else {
            continue;
        };
        let rest = rest.trim().trim_start_matches(':').trim();
        let value = rest.trim_matches(|c| c == '"' || c == ',');
        if !value.is_empty() {
            return value.to_string();
        }
    }
    panic!("no version in {}", path.display());
}

/// `[package] version` from Cargo.toml, before later dependency version keys.
fn package_version(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_package {
                break;
            }
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("version") else {
            continue;
        };
        let rest = rest.trim().trim_start_matches('=').trim();
        let value = rest.trim_matches('"');
        if !value.is_empty() {
            return value.to_string();
        }
    }
    panic!("no [package] version in {}", path.display());
}
