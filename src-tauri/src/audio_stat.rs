//! Size and modified time of a music file, without reading its contents.
//!
//! The beat cache keys a song by path + size + mtime, so a cache hit never reads the file.
//! Only audio files the app has registered for playback (the Music pane registers each song
//! it adds or restores) may be stat'ed; anything else is an error.
//!
//! Every error is one of the fixed strings below. None carries the input path or OS text,
//! so a failed stat never echoes a path back to the page.

use std::path::Path;
use std::time::UNIX_EPOCH;

/// Same list as `AUDIO_MIME` in slidex-core.js.
pub const AUDIO_EXTS: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus"];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AudioStat {
    pub size: u64,
    pub mtime_ms: u64,
}

pub fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .map_or(false, |e| AUDIO_EXTS.contains(&e.as_str()))
}

pub const ERR_NOT_AUDIO: &str = "not an audio file";
pub const ERR_NOT_RESOLVABLE: &str = "path not resolvable";
pub const ERR_NOT_ALLOWED: &str = "path not in allowed set";
pub const ERR_NOT_FILE: &str = "not a file";
pub const ERR_STAT_FAILED: &str = "stat failed";

/// The `audio_file_stat` route: resolve `raw`, then stat it. Fixed error strings only.
pub fn stat_audio_path(raw: &Path, allowed: impl Fn(&Path) -> bool) -> Result<AudioStat, String> {
    if !is_audio_path(raw) {
        return Err(ERR_NOT_AUDIO.into());
    }
    let canonical = raw.canonicalize().map_err(|_| ERR_NOT_RESOLVABLE.to_string())?;
    stat_audio(&canonical, allowed)
}

/// `canonical` must already be canonicalized; `allowed` says whether the app plays it.
pub fn stat_audio(canonical: &Path, allowed: impl Fn(&Path) -> bool) -> Result<AudioStat, String> {
    if !is_audio_path(canonical) {
        return Err(ERR_NOT_AUDIO.into());
    }
    if !allowed(canonical) {
        return Err(ERR_NOT_ALLOWED.into());
    }
    let meta = std::fs::metadata(canonical).map_err(|_| ERR_STAT_FAILED.to_string())?;
    if !meta.is_file() {
        return Err(ERR_NOT_FILE.into());
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(AudioStat { size: meta.len(), mtime_ms })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("slidex-audio-stat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        p.canonicalize().unwrap()
    }

    #[test]
    fn stats_an_allowed_song_without_reading_it() {
        let p = temp_file("song.MP3", &[7u8; 1234]);
        let s = stat_audio(&p, |q| q == p.as_path()).unwrap();
        assert_eq!(s.size, 1234);
        assert!(s.mtime_ms > 1_600_000_000_000, "mtime {}", s.mtime_ms);
    }

    #[test]
    fn refuses_paths_the_app_does_not_play() {
        let p = temp_file("other.flac", b"x");
        assert_eq!(stat_audio(&p, |_| false).unwrap_err(), "path not in allowed set");
    }

    #[test]
    fn refuses_non_audio_files_even_if_allowed() {
        let p = temp_file("notes.txt", b"x");
        assert_eq!(stat_audio(&p, |_| true).unwrap_err(), "not an audio file");
        let d = std::env::temp_dir().join(format!("slidex-audio-dir-{}.mp3", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(stat_audio(&d.canonicalize().unwrap(), |_| true).unwrap_err(), "not a file");
    }

    /// No piece of the input path (any component longer than 2 chars) is in the error.
    fn assert_no_path_leak(err: &str, input: &Path) {
        assert!(
            [ERR_NOT_AUDIO, ERR_NOT_RESOLVABLE, ERR_NOT_ALLOWED, ERR_NOT_FILE, ERR_STAT_FAILED].contains(&err),
            "unexpected error text: {err}"
        );
        let full = input.to_string_lossy().to_string();
        assert!(!err.contains(&full), "error {err:?} contains the path");
        for part in input.iter() {
            let part = part.to_string_lossy();
            let part = part.trim_matches(|c| c == '/' || c == '\\');
            if part.len() > 2 {
                assert!(!err.contains(part), "error {err:?} contains path part {part:?}");
            }
        }
        let stem = input.file_stem().unwrap().to_string_lossy().to_string();
        assert!(!err.contains(&stem), "error {err:?} contains file stem {stem:?}");
    }

    #[test]
    fn route_errors_never_echo_the_path() {
        let dir = std::env::temp_dir().join(format!("slidex-secret-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Non-existent song: resolving fails with a fixed string.
        let missing = dir.join("private-song-name-zz9.mp3");
        let err = stat_audio_path(&missing, |_| true).unwrap_err();
        assert_eq!(err, ERR_NOT_RESOLVABLE);
        assert_no_path_leak(&err, &missing);
        // Existing but unregistered song: refused with a fixed string.
        let unreg = temp_file("unregistered-secret-track.flac", b"x");
        let err = stat_audio_path(&unreg, |_| false).unwrap_err();
        assert_eq!(err, ERR_NOT_ALLOWED);
        assert_no_path_leak(&err, &unreg);
        // A song that resolves and is allowed, then vanishes before metadata: fixed string.
        let gone = temp_file("vanishing-secret-track.mp3", b"x");
        std::fs::remove_file(&gone).unwrap();
        let err = stat_audio(&gone, |_| true).unwrap_err();
        assert_eq!(err, ERR_STAT_FAILED);
        assert_no_path_leak(&err, &gone);
        // Non-audio input never gets as far as resolving.
        let txt = dir.join("secret-notes.txt");
        let err = stat_audio_path(&txt, |_| true).unwrap_err();
        assert_eq!(err, ERR_NOT_AUDIO);
        assert_no_path_leak(&err, &txt);
        // The happy path still works through the route.
        let ok = temp_file("route-ok.wav", &[1u8; 77]);
        assert_eq!(stat_audio_path(&ok, |p| p == ok.as_path()).unwrap().size, 77);
    }

    #[test]
    fn audio_exts_match_the_web_list() {
        let core = include_str!("../../slidex-core.js");
        let start = core.find("var AUDIO_MIME = {").expect("AUDIO_MIME in slidex-core.js");
        let body = &core[start..start + core[start..].find("};").unwrap()];
        let mut web: Vec<&str> = body
            .split(|c: char| c == '{' || c == ',' || c == '\n')
            .filter_map(|kv| kv.split(':').next())
            .map(|k| k.trim())
            .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric()))
            .collect();
        web.sort();
        let mut ours = AUDIO_EXTS.to_vec();
        ours.sort();
        assert_eq!(web, ours);
    }
}
