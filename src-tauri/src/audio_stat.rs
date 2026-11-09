//! Size and modified time of a music file, without reading its contents.
//!
//! The beat cache keys a song by path + size + mtime, so a cache hit never reads the file.
//! Only audio files the app has registered for playback (the Music pane registers each song
//! it adds or restores) may be stat'ed; anything else is an error.

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

/// `canonical` must already be canonicalized; `allowed` says whether the app plays it.
pub fn stat_audio(canonical: &Path, allowed: impl Fn(&Path) -> bool) -> Result<AudioStat, String> {
    if !is_audio_path(canonical) {
        return Err("not an audio file".into());
    }
    if !allowed(canonical) {
        return Err("path not in allowed set".into());
    }
    let meta = std::fs::metadata(canonical).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
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
