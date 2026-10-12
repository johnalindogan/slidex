mod cast;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Serialize;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

/// Replaces Tauri's default WebView2 args, so the disable-features list must stay.
const HIGH_PERF_BROWSER_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --force_high_performance_gpu";

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "ico",
];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "webm", "m4v"];

/// Full recursive folder walk with sane media-extension filter.
/// Cap guards against pathological trees (network mounts, etc.).
const FOLDER_MAX_FILES: usize = 10_000;

#[derive(Default)]
struct LaunchState {
    /// Exact paths from Open-with / CLI (also mirrored into AllowedMedia).
    paths: Mutex<Vec<String>>,
}

#[derive(Default)]
struct AllowedMedia {
    /// Canonical absolute file paths the frontend may read.
    files: Mutex<HashSet<PathBuf>>,
    /// Canonical directory roots; any media under these may be read.
    roots: Mutex<Vec<PathBuf>>,
}

#[derive(Serialize)]
struct LaunchPathsPayload {
    paths: Vec<String>,
}

#[derive(Serialize)]
struct FolderMediaPayload {
    paths: Vec<String>,
    truncated: bool,
    root: String,
}

fn ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

#[allow(dead_code)]
fn is_image_path(path: &Path) -> bool {
    ext_lower(path)
        .map(|e| IMAGE_EXTS.iter().any(|x| x == &e))
        .unwrap_or(false)
}

fn is_media_path(path: &Path) -> bool {
    ext_lower(path)
        .map(|e| {
            IMAGE_EXTS.iter().any(|x| x == &e) || VIDEO_EXTS.iter().any(|x| x == &e)
        })
        .unwrap_or(false)
}

fn strip_surrounding_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 {
        let bytes = t.as_bytes();
        if (bytes[0] == b'"' && bytes[t.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[t.len() - 1] == b'\'')
        {
            return &t[1..t.len() - 1];
        }
    }
    t
}

/// Parse CLI / Open-with args into image/video paths.
/// Only treat args that start with `file:` as URLs — `Url::parse` would otherwise
/// treat Windows paths like `C:/foo.jpg` as scheme `"c"` and drop them.
/// The executable path is kept in the list and dropped by the media filter, so a
/// second-instance argv that still starts with the exe does not hide the first file.
fn parse_launch_arg_list<I, S>(args: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut files = Vec::new();
    for maybe_file in args {
        let arg = strip_surrounding_quotes(maybe_file.as_ref());
        if arg.is_empty() || arg.starts_with('-') {
            continue;
        }
        if arg.to_ascii_lowercase().starts_with("file:") {
            if let Ok(url) = url::Url::parse(arg) {
                if url.scheme() == "file" {
                    if let Ok(path) = url.to_file_path() {
                        files.push(path);
                    }
                }
            }
            continue;
        }
        files.push(PathBuf::from(arg));
    }
    files.into_iter().filter(|p| is_media_path(p)).collect()
}

fn parse_launch_args() -> Vec<PathBuf> {
    parse_launch_arg_list(std::env::args())
}

fn focus_or_restore_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn canonicalize_path(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|e| format!("canonicalize failed for {}: {e}", path.display()))
}

fn path_under_root(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

fn is_path_allowed(canonical: &Path, allowed: &AllowedMedia) -> bool {
    if let Ok(files) = allowed.files.lock() {
        if files.contains(canonical) {
            return true;
        }
    }
    if let Ok(roots) = allowed.roots.lock() {
        if roots.iter().any(|r| path_under_root(canonical, r)) {
            return true;
        }
    }
    false
}

fn allow_exact_paths(allowed: &AllowedMedia, paths: &[PathBuf]) {
    if let Ok(mut files) = allowed.files.lock() {
        for p in paths {
            let c = p.canonicalize().unwrap_or_else(|_| p.clone());
            files.insert(c);
        }
    }
}

fn allow_root(allowed: &AllowedMedia, root: PathBuf) {
    if let Ok(mut roots) = allowed.roots.lock() {
        let c = root.canonicalize().unwrap_or(root);
        if !roots.iter().any(|r| r == &c) {
            roots.push(c);
        }
    }
}

fn inject_launch_paths_to_webview(app: &AppHandle) {
    let paths = app
        .try_state::<LaunchState>()
        .and_then(|state| state.paths.lock().ok().map(|g| g.clone()))
        .unwrap_or_default();
    if paths.is_empty() {
        return;
    }
    let json = serde_json::to_string(&paths).unwrap_or_else(|_| "[]".to_string());
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.eval(&format!("window.__SSP_LAUNCH_PATHS__ = {json};"));
        let _ = win.emit("ssp-launch-paths", ());
    }
}

fn store_launch_paths(app: &AppHandle, files: Vec<PathBuf>) {
    let paths: Vec<String> = files
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()).to_string_lossy().into_owned())
        .collect();

    if let Some(state) = app.try_state::<LaunchState>() {
        if let Ok(mut guard) = state.paths.lock() {
            *guard = paths.clone();
        }
    }
    if let Some(allowed) = app.try_state::<AllowedMedia>() {
        allow_exact_paths(&allowed, &files);
    }

    let json = serde_json::to_string(&paths).unwrap_or_else(|_| "[]".to_string());
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.eval(&format!("window.__SSP_LAUNCH_PATHS__ = {json};"));
        // Notify the HTML bridge on all platforms (Emitter was previously mac/ios-only).
        let _ = win.emit("ssp-launch-paths", ());
    }
}

#[tauri::command]
fn get_launch_paths(state: State<'_, LaunchState>) -> LaunchPathsPayload {
    let paths = state.paths.lock().map(|g| g.clone()).unwrap_or_default();
    LaunchPathsPayload { paths }
}

#[tauri::command]
fn register_allowed_paths(
    paths: Vec<String>,
    allowed: State<'_, AllowedMedia>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut bufs = Vec::new();
    for p in paths {
        let pb = PathBuf::from(&p);
        let c = canonicalize_path(&pb).unwrap_or(pb);
        out.push(c.to_string_lossy().into_owned());
        bufs.push(c);
    }
    allow_exact_paths(&allowed, &bufs);
    Ok(out)
}

/// Recursively list image/video files under a user-picked folder (sane ext filter).
/// Registers the folder as an allowed root so `read_media_file` may load them.
#[tauri::command]
fn list_folder_media(
    path: String,
    allowed: State<'_, AllowedMedia>,
) -> Result<FolderMediaPayload, String> {
    let root = canonicalize_path(Path::new(&path))?;
    if !root.is_dir() {
        return Err("not a directory".into());
    }
    allow_root(&allowed, root.clone());

    let mut found: Vec<PathBuf> = Vec::new();
    let mut truncated = false;
    let mut stack = vec![root.clone()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if !ft.is_file() || !is_media_path(&p) {
                continue;
            }
            if found.len() >= FOLDER_MAX_FILES {
                truncated = true;
                break;
            }
            found.push(p);
        }
        if truncated {
            break;
        }
    }

    found.sort();
    let paths: Vec<String> = found
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    Ok(FolderMediaPayload {
        paths,
        truncated,
        root: root.to_string_lossy().into_owned(),
    })
}

/// Soft cap for base64 blob ingest — huge videos fall back to convertFileSrc in JS.
const BLOB_INGEST_MAX_BYTES: u64 = 80 * 1024 * 1024;

#[tauri::command]
fn media_file_size(path: String, allowed: State<'_, AllowedMedia>) -> Result<u64, String> {
    let canonical = canonicalize_path(Path::new(&path))?;
    if !is_path_allowed(&canonical, &allowed) {
        return Err("path not in allowed set".into());
    }
    let meta = std::fs::metadata(&canonical).map_err(|e| e.to_string())?;
    Ok(meta.len())
}

#[tauri::command]
fn read_media_file(path: String, allowed: State<'_, AllowedMedia>) -> Result<String, String> {
    let canonical = canonicalize_path(Path::new(&path))?;
    if !is_path_allowed(&canonical, &allowed) {
        return Err("path not in allowed set".into());
    }
    if !is_media_path(&canonical) {
        return Err("unsupported extension".into());
    }
    let meta = std::fs::metadata(&canonical).map_err(|e| e.to_string())?;
    if meta.len() > BLOB_INGEST_MAX_BYTES {
        return Err("file too large for blob ingest".into());
    }
    let bytes = std::fs::read(&canonical).map_err(|e| e.to_string())?;
    Ok(B64.encode(bytes))
}

/// Write an export file chosen via the native save dialog (graded still, WebM, or grade sidecar).
const EXPORT_MAX_BYTES: usize = 200 * 1024 * 1024;
const EXPORT_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "webm", "mp4", "mov", "json", "sspgrade",
];

#[tauri::command]
fn write_export_file(path: String, data_b64: String, exclusive: Option<bool>) -> Result<(), String> {
    let bytes = B64.decode(data_b64.as_bytes()).map_err(|e| format!("base64 decode: {e}"))?;
    if bytes.len() > EXPORT_MAX_BYTES {
        return Err("export too large".into());
    }
    let pb = PathBuf::from(&path);
    let ext = ext_lower(&pb).unwrap_or_default();
    if !EXPORT_EXTS.iter().any(|x| *x == ext) {
        return Err(format!("unsupported export extension: {ext}"));
    }
    if let Some(parent) = pb.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    if exclusive.unwrap_or(false) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pb)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    "file already exists".to_string()
                } else {
                    e.to_string()
                }
            })?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        Ok(())
    } else {
        std::fs::write(&pb, &bytes).map_err(|e| e.to_string())
    }
}

fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    atomic_write_with(path, bytes, |from, to| std::fs::rename(from, to))
}

/// Write `bytes` to a temp file in the same directory, `sync_all`, then rename over `path`.
/// `rename` replaces an existing target (on Windows, MoveFileExW with MOVEFILE_REPLACE_EXISTING).
/// The previous file is never deleted first, so a crash or a failed rename leaves it intact.
fn atomic_write_with<R>(path: &Path, bytes: &[u8], rename_fn: R) -> Result<(), String>
where
    R: Fn(&Path, &Path) -> std::io::Result<()>,
{
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).ok_or("no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("bad file name")?;
    let tmp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    if let Err(err) = rename_fn(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(())
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

fn crop_beside_path(media_path: &str) -> Result<PathBuf, String> {
    if media_path.trim().is_empty() {
        return Err("missing media path".into());
    }
    Ok(PathBuf::from(format!("{media_path}.sspcrop.json")))
}

fn crop_appdata_path(app: &AppHandle, media_path: &str, name: &str) -> Result<PathBuf, String> {
    let base = app_data_dir(app)?.join("crops");
    let key = format!("{media_path}|{name}");
    let stem = Path::new(if name.is_empty() { media_path } else { name })
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("media");
    let safe: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    Ok(base.join(format!("{:016x}_{safe}.sspcrop.json", fnv1a64(&key))))
}

fn crop_value_from_text(text: &str) -> Option<serde_json::Value> {
    let doc: serde_json::Value = serde_json::from_str(text).ok()?;
    if doc.get("sspCropVersion").and_then(|v| v.as_u64()) != Some(1) {
        return None;
    }
    let crop = doc.get("crop")?.as_object()?;
    for key in ["x", "y", "w", "h"] {
        let n = crop.get(key)?.as_f64()?;
        if !n.is_finite() {
            return None;
        }
    }
    Some(doc.get("crop").cloned().unwrap_or(serde_json::Value::Null))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CropReadResult {
    crop: Option<serde_json::Value>,
    location: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CropWriteResult {
    location: String,
    path: String,
    message: Option<String>,
}

#[tauri::command]
fn read_crop_sidecar(app: AppHandle, path: String, name: Option<String>) -> Result<CropReadResult, String> {
    let name = name.unwrap_or_default();
    if let Ok(beside) = crop_beside_path(&path) {
        if let Ok(text) = std::fs::read_to_string(&beside) {
            if let Some(crop) = crop_value_from_text(&text) {
                return Ok(CropReadResult { crop: Some(crop), location: Some("beside".into()) });
            }
        }
    }
    if let Ok(fallback) = crop_appdata_path(&app, &path, &name) {
        if let Ok(text) = std::fs::read_to_string(&fallback) {
            if let Some(crop) = crop_value_from_text(&text) {
                return Ok(CropReadResult { crop: Some(crop), location: Some("appdata".into()) });
            }
        }
    }
    Ok(CropReadResult { crop: None, location: None })
}

#[tauri::command]
fn write_crop_sidecar(
    app: AppHandle,
    path: String,
    name: Option<String>,
    document_json: String,
) -> Result<CropWriteResult, String> {
    if crop_value_from_text(&document_json).is_none() {
        return Err("invalid crop sidecar".into());
    }
    let bytes = document_json.as_bytes();
    let name = name.unwrap_or_default();
    if let Ok(beside) = crop_beside_path(&path) {
        if atomic_write(&beside, bytes).is_ok() {
            return Ok(CropWriteResult {
                location: "beside".into(),
                path: beside.to_string_lossy().into_owned(),
                message: None,
            });
        }
    }
    let fallback = crop_appdata_path(&app, &path, &name)?;
    atomic_write(&fallback, bytes)?;
    Ok(CropWriteResult {
        location: "appdata".into(),
        path: fallback.to_string_lossy().into_owned(),
        message: Some("The folder isn't writable. Crop saved in app data.".into()),
    })
}

#[tauri::command]
fn delete_crop_sidecar(app: AppHandle, path: String, name: Option<String>) -> Result<(), String> {
    let name = name.unwrap_or_default();
    if let Ok(beside) = crop_beside_path(&path) {
        if beside.exists() {
            let _ = std::fs::remove_file(beside);
        }
    }
    if let Ok(fallback) = crop_appdata_path(&app, &path, &name) {
        if fallback.exists() {
            let _ = std::fs::remove_file(fallback);
        }
    }
    Ok(())
}

fn playlist_slug(name: &str) -> String {
    let key = name.trim().to_lowercase();
    let mut slug = String::new();
    for c in key.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if c == ' ' || c == '-' || c == '_' {
            if !slug.ends_with('-') {
                slug.push('-');
            }
        }
    }
    let slug = slug.trim_matches('-');
    let base = if slug.is_empty() { "playlist" } else { slug };
    format!("{base}-{:016x}", fnv1a64(&key))
}

fn playlists_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app_data_dir(app)?.join("playlists");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn playlist_file(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    let dir = playlists_dir(app)?;
    let path = dir.join(format!("{}.json", playlist_slug(name)));
    if !path.starts_with(&dir) {
        return Err("playlist path escaped app data".into());
    }
    Ok(path)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NamedPlaylistInfo {
    name: String,
    count: usize,
    saved_at: Option<serde_json::Value>,
    file: String,
}

#[tauri::command]
fn save_named_playlist(app: AppHandle, name: String, json: String, replace: Option<bool>) -> Result<NamedPlaylistInfo, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("playlist name required".into());
    }
    let mut doc: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let items = doc.get("items").and_then(|v| v.as_array()).ok_or("invalid playlist")?;
    let count = items.iter().filter(|it| it.get("type").and_then(|t| t.as_str()) != Some("folder")).count();
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("name".into(), serde_json::Value::String(trimmed.to_string()));
        obj.entry("sspVersion").or_insert(serde_json::Value::from(1));
        obj.entry("version").or_insert(serde_json::Value::from(2));
    }
    let path = playlist_file(&app, trimmed)?;
    if path.exists() && !replace.unwrap_or(false) {
        return Err("playlist already exists".into());
    }
    let text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    atomic_write(&path, text.as_bytes())?;
    Ok(NamedPlaylistInfo {
        name: trimmed.to_string(),
        count,
        saved_at: doc.get("savedAt").cloned(),
        file: path.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
fn list_named_playlists(app: AppHandle) -> Result<Vec<NamedPlaylistInfo>, String> {
    let dir = playlists_dir(&app)?;
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let doc: serde_json::Value = match serde_json::from_str(&text) {
            Ok(doc) => doc,
            Err(_) => continue,
        };
        let Some(items) = doc.get("items").and_then(|v| v.as_array()) else { continue };
        let name = doc.get("name").and_then(|v| v.as_str()).unwrap_or("Playlist").to_string();
        let count = items.iter().filter(|it| it.get("type").and_then(|t| t.as_str()) != Some("folder")).count();
        out.push(NamedPlaylistInfo {
            name,
            count,
            saved_at: doc.get("savedAt").cloned(),
            file: path.to_string_lossy().into_owned(),
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

#[tauri::command]
fn read_named_playlist(app: AppHandle, name: String) -> Result<String, String> {
    let path = playlist_file(&app, name.trim())?;
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_named_playlist(app: AppHandle, from: String, to: String) -> Result<NamedPlaylistInfo, String> {
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() || to.is_empty() {
        return Err("playlist name required".into());
    }
    let src = playlist_file(&app, from)?;
    let text = std::fs::read_to_string(&src).map_err(|e| e.to_string())?;
    let dest = playlist_file(&app, to)?;
    if dest != src && dest.exists() {
        return Err("playlist already exists".into());
    }
    let mut doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("name".into(), serde_json::Value::String(to.to_string()));
    }
    let pretty = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    atomic_write(&dest, pretty.as_bytes())?;
    if dest != src {
        let _ = std::fs::remove_file(src);
    }
    let count = doc.get("items").and_then(|v| v.as_array()).map(|items| {
        items.iter().filter(|it| it.get("type").and_then(|t| t.as_str()) != Some("folder")).count()
    }).unwrap_or(0);
    Ok(NamedPlaylistInfo {
        name: to.to_string(),
        count,
        saved_at: doc.get("savedAt").cloned(),
        file: dest.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
fn delete_named_playlist(app: AppHandle, name: String) -> Result<(), String> {
    let path = playlist_file(&app, name.trim())?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// One offline UNC share must not stall Continue. Each root is probed once.
const PATH_PROBE_TIMEOUT: Duration = Duration::from_millis(2500);

/// `\\?\C:\...` becomes `C:\...`. `\\?\UNC\server\share\...` becomes `\\server\share\...`.
/// The saved session string is left unchanged; only probing uses this form.
fn normalize_extended_path(path: &str) -> String {
    let trimmed = path.trim().replace('/', "\\");
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") {
        return format!(r"\\{}", &trimmed[8..]);
    }
    if lower.starts_with(r"\\?\") {
        let rest = &trimmed[4..];
        let mut chars = rest.chars();
        if let (Some(drive), Some(':')) = (chars.next(), chars.next()) {
            if drive.is_ascii_alphabetic() {
                return rest.to_string();
            }
        }
    }
    trimmed
}

/// UNC `\\server\share` or a drive such as `D:\`. Other paths are their own root.
/// Extended-length prefixes are stripped before the root is chosen.
fn share_root(path: &str) -> String {
    let normalized = normalize_extended_path(path);
    if let Some(rest) = normalized.strip_prefix("\\\\") {
        let mut parts = rest.split('\\').filter(|s| !s.is_empty());
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        if !server.is_empty() && !share.is_empty() {
            return format!(
                "\\\\{}\\{}",
                server.to_ascii_lowercase(),
                share.to_ascii_lowercase()
            );
        }
        if !server.is_empty() {
            return format!("\\\\{}", server.to_ascii_lowercase());
        }
    }
    let mut chars = normalized.chars();
    if let (Some(drive), Some(':')) = (chars.next(), chars.next()) {
        if drive.is_ascii_alphabetic() {
            return format!("{}:\\", drive.to_ascii_uppercase());
        }
    }
    normalized
}

/// Probe each share root once, in parallel. A root that is unreachable or does not
/// answer within `timeout` marks every path on that root missing, with no per-file stat.
fn paths_exist_grouped<R, F>(paths: &[String], timeout: Duration, probe_root: R, probe_file: F) -> Vec<bool>
where
    R: Fn(&str) -> bool + Send + Sync + 'static,
    F: Fn(&str) -> bool + Send + Sync + 'static,
{
    let mut out = vec![false; paths.len()];
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if path.trim().is_empty() {
            continue;
        }
        let probe_path = normalize_extended_path(path);
        let root = share_root(&probe_path);
        if let Some(group) = groups.iter_mut().find(|(name, _)| name == &root) {
            group.1.push(index);
        } else {
            groups.push((root, vec![index]));
        }
    }
    if groups.is_empty() {
        return out;
    }

    let probe_root = Arc::new(probe_root);
    let probe_file = Arc::new(probe_file);
    let mut receivers = Vec::with_capacity(groups.len());
    for (root, indexes) in groups {
        let (tx, rx) = mpsc::channel::<Vec<(usize, bool)>>();
        let root_probe = Arc::clone(&probe_root);
        let file_probe = Arc::clone(&probe_file);
        let owned: Vec<(usize, String)> = indexes
            .iter()
            .map(|i| (*i, normalize_extended_path(&paths[*i])))
            .collect();
        thread::spawn(move || {
            if !root_probe(&root) {
                let _ = tx.send(owned.into_iter().map(|(i, _)| (i, false)).collect());
                return;
            }
            let flags = owned
                .into_iter()
                .map(|(i, path)| (i, file_probe(&path)))
                .collect();
            let _ = tx.send(flags);
        });
        receivers.push(rx);
    }

    let deadline = Instant::now() + timeout;
    for rx in receivers {
        let remain = deadline.saturating_duration_since(Instant::now());
        if let Ok(flags) = rx.recv_timeout(remain) {
            for (index, exists) in flags {
                if index < out.len() {
                    out[index] = exists;
                }
            }
        }
    }
    out
}

fn paths_exist_blocking(paths: Vec<String>) -> Vec<bool> {
    paths_exist_grouped(
        &paths,
        PATH_PROBE_TIMEOUT,
        |root| Path::new(root).exists(),
        |path| Path::new(path).is_file(),
    )
}

#[tauri::command]
async fn paths_exist(paths: Vec<String>) -> Result<Vec<bool>, String> {
    tauri::async_runtime::spawn_blocking(move || paths_exist_blocking(paths))
        .await
        .map_err(|e| format!("path check failed: {e}"))
}

#[cfg(test)]
mod slidex_store_tests {
    use super::*;

    #[test]
    fn slow_share_root_times_out_and_is_missing_without_file_stats() {
        let paths = vec![
            r"\\AX03\share\a.jpg".to_string(),
            r"\\AX03\share\b.jpg".to_string(),
        ];
        let file_stats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&file_stats);
        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let flags = paths_exist_grouped(
            &paths,
            timeout,
            |_| {
                thread::sleep(Duration::from_millis(1500));
                true
            },
            move |_| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            },
        );
        let elapsed = started.elapsed();
        assert_eq!(flags, vec![false, false]);
        assert_eq!(file_stats.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            elapsed < Duration::from_millis(800),
            "slow root held the check for {elapsed:?}"
        );
        assert!(PATH_PROBE_TIMEOUT <= Duration::from_millis(2500));
        assert!(PATH_PROBE_TIMEOUT >= Duration::from_millis(2000));
    }

    #[test]
    fn fast_share_root_is_statted_and_an_unreachable_root_is_not() {
        let paths = vec![
            r"D:\keep.jpg".to_string(),
            r"D:\gone.jpg".to_string(),
            r"\\AX03\share\clip.mp4".to_string(),
            r"\\AX03\share\still.jpg".to_string(),
        ];
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_probe = Arc::clone(&seen);
        let timeout = Duration::from_millis(400);
        let started = Instant::now();
        let flags = paths_exist_grouped(
            &paths,
            timeout,
            |root| {
                if root.to_ascii_lowercase().contains("ax03") {
                    thread::sleep(Duration::from_millis(1500));
                    return true;
                }
                true
            },
            move |path| {
                seen_probe.lock().unwrap().push(path.to_string());
                path.ends_with("keep.jpg")
            },
        );
        let elapsed = started.elapsed();
        assert_eq!(flags, vec![true, false, false, false]);
        let mut probed = seen.lock().unwrap().clone();
        probed.sort();
        assert_eq!(probed, vec![r"D:\gone.jpg".to_string(), r"D:\keep.jpg".to_string()]);
        assert!(
            elapsed < Duration::from_millis(900),
            "mixed roots held the check for {elapsed:?}"
        );
        assert_eq!(share_root(r"\\AX03\Share\a.jpg"), share_root(r"//ax03/share/b.jpg"));
        assert_eq!(share_root(r"d:/photos/a.jpg"), r"D:\");
    }

    #[test]
    fn extended_prefix_paths_probe_present_and_the_saved_strings_stay() {
        assert_eq!(share_root(r"\\?\C:\Users\qa\Pictures\a.jpg"), r"C:\");
        assert_eq!(share_root(r"C:\Users\qa\Pictures\a.jpg"), r"C:\");
        assert_eq!(share_root(r"\\?\UNC\AX03\photos\b.jpg"), r"\\ax03\photos");
        assert_eq!(share_root(r"\\AX03\photos\b.jpg"), r"\\ax03\photos");
        assert_eq!(
            normalize_extended_path(r"\\?\C:\Users\qa\Pictures\a.jpg"),
            r"C:\Users\qa\Pictures\a.jpg"
        );
        assert_eq!(
            normalize_extended_path(r"\\?\UNC\AX03\photos\b.jpg"),
            r"\\AX03\photos\b.jpg"
        );

        let paths = vec![
            r"\\?\C:\Users\qa\Pictures\lake.jpg".to_string(),
            r"\\?\UNC\AX03\photos\dock.jpg".to_string(),
            r"C:\Users\qa\Pictures\plain.jpg".to_string(),
            r"\\AX03\photos\plain.jpg".to_string(),
        ];
        let saved = paths.clone();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_probe = Arc::clone(&seen);
        let flags = paths_exist_grouped(
            &paths,
            Duration::from_millis(500),
            |_| true,
            move |path| {
                seen_probe.lock().unwrap().push(path.to_string());
                !path.starts_with(r"\\?\")
            },
        );
        assert_eq!(flags, vec![true, true, true, true]);
        assert_eq!(paths, saved);
        let mut probed = seen.lock().unwrap().clone();
        probed.sort();
        assert_eq!(
            probed,
            vec![
                r"C:\Users\qa\Pictures\lake.jpg".to_string(),
                r"C:\Users\qa\Pictures\plain.jpg".to_string(),
                r"\\AX03\photos\dock.jpg".to_string(),
                r"\\AX03\photos\plain.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn fnv_is_stable_and_not_rust_hasher() {
        assert_eq!(fnv1a64("G:\\Photos\\a.jpg"), fnv1a64("G:\\Photos\\a.jpg"));
        assert_ne!(fnv1a64("a"), fnv1a64("b"));
        assert_eq!(fnv1a64(""), 0xcbf29ce484222325);
    }

    #[test]
    fn atomic_write_replaces_without_leaving_a_partial_file() {
        let dir = std::env::temp_dir().join(format!("slidex-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("playlists").join("demo.json");
        atomic_write(&path, b"{\"ok\":1}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"ok\":1}");
        atomic_write(&path, b"{\"ok\":2}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"ok\":2}");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp")).collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_over_a_playlist_keeps_complete_old_content_when_rename_fails() {
        let dir = std::env::temp_dir().join(format!("slidex-atomic-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("playlists").join("Beach.json");
        let old = br#"{"name":"Beach","items":[{"id":"a","type":"image","name":"a.jpg"}]}"#;
        let new = br#"{"name":"Beach","items":[{"id":"b","type":"image","name":"b.jpg"}]}"#;
        atomic_write(&path, old).unwrap();
        let err = atomic_write_with(&path, new, |_from, _to| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "simulated rename failure",
            ))
        });
        assert!(err.is_err());
        assert!(path.is_file());
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, old, "failed rename must leave the complete old playlist");
        assert_ne!(on_disk, new);
        assert!(on_disk.starts_with(b"{") && on_disk.ends_with(b"}"));
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a failed rename must not leave a partial temp file");
        atomic_write(&path, new).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), new);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlist_slug_ignores_case_and_keeps_distinct_names_apart() {
        assert_eq!(playlist_slug("Beach"), playlist_slug("beach"));
        assert_eq!(playlist_slug("Beach"), playlist_slug("BEACH"));
        assert_eq!(playlist_slug(" Beach "), playlist_slug("beach"));
        assert_ne!(playlist_slug("Beach"), playlist_slug("Shore"));
    }

    #[test]
    fn second_launch_forwards_open_with_paths_and_drops_the_exe() {
        let files = parse_launch_arg_list([
            r"C:\Program Files\SlideX\slideshowpro.exe",
            r"C:\Photos\Beach.jpg",
            "--flag",
            r"D:\clip.mp4",
            "notes.txt",
            "file:///tmp/other.png",
        ]);
        assert_eq!(files.len(), 3);
        let shown: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert!(shown[0].ends_with("Beach.jpg"), "{}", shown[0]);
        assert!(shown[1].ends_with("clip.mp4"), "{}", shown[1]);
        assert!(shown[2].ends_with("other.png"), "{}", shown[2]);
    }

    #[test]
    fn gpu_args_keep_the_default_disable_features_list() {
        assert!(HIGH_PERF_BROWSER_ARGS.contains(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection"
        ));
        assert!(HIGH_PERF_BROWSER_ARGS.contains("--force_high_performance_gpu"));
        let conf = include_str!("../tauri.conf.json");
        assert!(conf.contains(HIGH_PERF_BROWSER_ARGS));
    }

    #[test]
    fn close_waits_for_save_ack_or_two_second_timeout_and_ignores_reentry() {
        assert!(SAVE_BEFORE_EXIT_TIMEOUT <= Duration::from_secs(2));
        assert_eq!(SAVE_BEFORE_EXIT_TIMEOUT.as_millis(), 2000);

        let gate = ExitSaveGate::new();
        assert_eq!(gate.on_close_requested(), ExitCloseAction::BeginSave);
        assert_eq!(gate.on_close_requested(), ExitCloseAction::AlreadyWaiting);
        assert!(!gate.should_exit(ExitSignal::Timeout, 1999));
        assert!(gate.should_exit(ExitSignal::Timeout, 2000));
        assert!(!gate.should_exit(ExitSignal::Ack, 0));
        assert!(!gate.should_exit(ExitSignal::Timeout, 5000));

        let acked = ExitSaveGate::new();
        assert_eq!(acked.on_close_requested(), ExitCloseAction::BeginSave);
        assert!(acked.should_exit(ExitSignal::Ack, 0));
        assert!(!acked.should_exit(ExitSignal::Timeout, 2000));
    }
}

#[tauri::command]
fn read_text_file(path: String, allowed: State<'_, AllowedMedia>) -> Result<String, String> {
    let canonical = canonicalize_path(Path::new(&path))?;
    if !is_path_allowed(&canonical, &allowed) {
        return Err("path not in allowed set".into());
    }
    let meta = std::fs::metadata(&canonical).map_err(|e| e.to_string())?;
    if meta.len() > 8 * 1024 * 1024 {
        return Err("text file too large".into());
    }
    std::fs::read_to_string(&canonical).map_err(|e| e.to_string())
}


/// Open (or focus) the undocked Media Manager window.
///
/// Must return promptly: on Windows, calling `WebviewWindowBuilder::build()`
/// from a *sync* IPC command deadlocks (WebView2). Even in an async command,
/// awaiting `build()` can leave the frontend `invoke` hanging. Schedule create
/// on the async runtime and resolve the invoke immediately so main can set
/// `playlistUndocked` / hide the dock without waiting for the child to load.
#[tauri::command]
async fn open_playlist_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("playlist") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(w) = handle.get_webview_window("playlist") {
            let _ = w.show();
            let _ = w.set_focus();
            return;
        }
        if let Err(e) = WebviewWindowBuilder::new(
            &handle,
            "playlist",
            WebviewUrl::App("index.html?sspWindow=playlist#sspWindow=playlist".into()),
        )
        .title("SlideX — Media Manager")
        .inner_size(560.0, 820.0)
        .min_inner_size(360.0, 420.0)
        .resizable(true)
        .focused(true)
        .additional_browser_args(HIGH_PERF_BROWSER_ARGS)
        .build()
        {
            eprintln!("[slideshowpro] open_playlist_window build failed: {e}");
        }
    });

    Ok(())
}

#[tauri::command]
fn close_playlist_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("playlist") {
        w.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn focus_playlist_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("playlist") {
        let _ = w.show();
        let _ = w.set_focus();
        Ok(())
    } else {
        Err("playlist window not open".into())
    }
}


#[tauri::command]
fn cast_discover(timeout_ms: Option<u64>) -> Result<cast::CastDiscoverResult, String> {
    cast::discover_devices(timeout_ms)
}

#[tauri::command]
fn cast_network_fingerprint() -> Result<String, String> {
    Ok(cast::network_fingerprint())
}

#[tauri::command]
fn cast_connect(
    state: State<'_, cast::CastState>,
    host: String,
    port: Option<u16>,
    name: Option<String>,
) -> Result<cast::CastConnectInfo, String> {
    state.connect(&host, port, name)
}

#[tauri::command]
fn cast_set_playlist(
    state: State<'_, cast::CastState>,
    paths: Vec<String>,
) -> Result<cast::PlaylistUpdate, String> {
    state.set_playlist(paths)
}

#[tauri::command]
fn cast_load(
    state: State<'_, cast::CastState>,
    path: String,
    autoplay: Option<bool>,
    source: Option<String>,
) -> Result<(), String> {
    let origin = cast::CastOrigin::parse(source.as_deref().unwrap_or("user"))?;
    state.load(path, autoplay.unwrap_or(true), origin)
}

#[tauri::command]
fn cast_pause(state: State<'_, cast::CastState>, source: Option<String>) -> Result<(), String> {
    state.pause(cast::CastOrigin::parse(source.as_deref().unwrap_or("user"))?)
}

#[tauri::command]
fn cast_play(state: State<'_, cast::CastState>, source: Option<String>) -> Result<(), String> {
    state.play(cast::CastOrigin::parse(source.as_deref().unwrap_or("user"))?)
}

#[tauri::command]
fn cast_disconnect(state: State<'_, cast::CastState>) -> Result<(), String> {
    state.disconnect()
}

#[tauri::command]
fn cast_status(state: State<'_, cast::CastState>) -> Result<cast::CastLiveStatus, String> {
    Ok(state.status())
}

#[tauri::command]
fn cast_firewall_status() -> Result<cast::CastFirewallStatus, String> {
    Ok(cast::firewall_status())
}

#[tauri::command]
fn cast_log_path() -> Result<String, String> {
    Ok(cast::cast_log_path().to_string_lossy().into_owned())
}

/// Ask the page to save, then exit. Never wait longer than this.
const SAVE_BEFORE_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const SAVE_BEFORE_EXIT_EVENT: &str = "slidex://save-before-exit";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitPhase {
    Idle,
    Waiting,
    Exiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitSignal {
    Ack,
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitCloseAction {
    BeginSave,
    AlreadyWaiting,
}

struct ExitSaveGate {
    phase: Mutex<ExitPhase>,
}

impl ExitSaveGate {
    fn new() -> Self {
        Self {
            phase: Mutex::new(ExitPhase::Idle),
        }
    }

    fn on_close_requested(&self) -> ExitCloseAction {
        let mut phase = self.phase.lock().expect("exit save gate");
        if *phase == ExitPhase::Idle {
            *phase = ExitPhase::Waiting;
            ExitCloseAction::BeginSave
        } else {
            ExitCloseAction::AlreadyWaiting
        }
    }

    /// Ack always finishes a wait. Timeout finishes only at >= 2s.
    /// A second signal does not exit again.
    fn should_exit(&self, signal: ExitSignal, elapsed_ms: u64) -> bool {
        let mut phase = self.phase.lock().expect("exit save gate");
        if *phase != ExitPhase::Waiting {
            return false;
        }
        let ready = match signal {
            ExitSignal::Ack => true,
            ExitSignal::Timeout => elapsed_ms >= SAVE_BEFORE_EXIT_TIMEOUT.as_millis() as u64,
        };
        if ready {
            *phase = ExitPhase::Exiting;
            true
        } else {
            false
        }
    }
}

fn finish_exit_save(app: &AppHandle, signal: ExitSignal) {
    let elapsed = match signal {
        ExitSignal::Ack => 0,
        ExitSignal::Timeout => SAVE_BEFORE_EXIT_TIMEOUT.as_millis() as u64,
    };
    let Some(gate) = app.try_state::<ExitSaveGate>() else {
        return;
    };
    if gate.should_exit(signal, elapsed) {
        app.exit(0);
    }
}

#[tauri::command]
fn slidex_save_done(app: AppHandle) {
    finish_exit_save(&app, ExitSignal::Ack);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            focus_or_restore_main(app);
            let files = parse_launch_arg_list(args);
            if !files.is_empty() {
                store_launch_paths(app, files);
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(LaunchState::default())
        .manage(AllowedMedia::default())
        .manage(cast::CastState::default())
        .manage(ExitSaveGate::new())
        .invoke_handler(tauri::generate_handler![
            get_launch_paths,
            read_media_file,
            media_file_size,
            register_allowed_paths,
            list_folder_media,
            read_text_file,
            open_playlist_window,
            close_playlist_window,
            focus_playlist_window,
            write_export_file,
            read_crop_sidecar,
            write_crop_sidecar,
            delete_crop_sidecar,
            save_named_playlist,
            list_named_playlists,
            read_named_playlist,
            rename_named_playlist,
            delete_named_playlist,
            paths_exist,
            cast_discover,
            cast_network_fingerprint,
            cast_connect,
            cast_set_playlist,
            cast_load,
            cast_pause,
            cast_play,
            cast_disconnect,
            cast_status,
            cast_firewall_status,
            cast_log_path,
            slidex_save_done
        ])
        .setup(|app| {
            #[cfg(any(windows, target_os = "linux"))]
            {
                let files = parse_launch_args();
                if !files.is_empty() {
                    store_launch_paths(app.handle(), files);
                }
            }
            Ok(())
        })
        // Re-inject after the page finishes loading — setup/eval can race the webview.
        .on_page_load(|webview, payload| {
            if payload.event() != PageLoadEvent::Finished {
                return;
            }
            if webview.label() == "playlist" {
                // Ensure JS role even if asset URL drops ?sspWindow= (installed builds).
                let _ = webview.eval(
                    "try{window.__SSP_WINDOW_ROLE='playlist';}catch(_e){}",
                );
                return;
            }
            if webview.label() != "main" {
                return;
            }
            inject_launch_paths_to_webview(webview.app_handle());
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Keep the window up until the page saves or the timeout fires.
                // A second close while waiting must not emit again or exit twice.
                // The Media Manager may be open or hidden; exiting the process
                // still ends it once the save finishes.
                api.prevent_close();
                let gate = window.state::<ExitSaveGate>();
                if gate.on_close_requested() == ExitCloseAction::BeginSave {
                    let _ = window.emit(SAVE_BEFORE_EXIT_EVENT, ());
                    let app = window.app_handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(SAVE_BEFORE_EXIT_TIMEOUT);
                        finish_exit_save(&app, ExitSignal::Timeout);
                    });
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = &event {
                if let Some(state) = app.try_state::<cast::CastState>() {
                    state.shutdown();
                }
            }
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            if let tauri::RunEvent::Opened { urls } = &event {
                let files: Vec<PathBuf> = urls
                    .iter()
                    .filter_map(|u| u.to_file_path().ok())
                    .filter(|p| is_media_path(p))
                    .collect();
                if !files.is_empty() {
                    store_launch_paths(app, files);
                }
            }
            let _ = (app, &event);
        });
}
