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

    tauri_build::build()
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
