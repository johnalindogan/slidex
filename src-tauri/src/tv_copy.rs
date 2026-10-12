//! Background H.264 copy for a video the TV will not play as-is.
//!
//! The bundled ffmpeg lives beside the app, never on PATH. The video encoder
//! is one setting (`libx264` by default). `h264_mf`, `h264_nvenc`, and
//! `h264_qsv` are the alternatives, so a later LGPL build can switch encoders
//! without another code path.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{cast_log, decide_cast_video, fits_box, item_log_token};

/// Shown on the cast banner while a TV copy is being built.
pub(crate) const PREPARING_TV: &str = "Preparing for TV…";

/// Copies live under `%LOCALAPPDATA%\com.johnalindogan.slideshowpro\tv-cache`.
const CACHE_LIMIT_DEFAULT: u64 = 20 * 1024 * 1024 * 1024;
const UHD_W: u32 = 3840;
const UHD_H: u32 = 2160;
const MAX_LONG_SIDE: u32 = 2160;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TvEncoder {
    Libx264,
    H264Mf,
    H264Nvenc,
    H264Qsv,
}

impl TvEncoder {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Libx264 => "libx264",
            Self::H264Mf => "h264_mf",
            Self::H264Nvenc => "h264_nvenc",
            Self::H264Qsv => "h264_qsv",
        }
    }
}

pub(crate) fn parse_encoder(name: &str) -> Option<TvEncoder> {
    match name.trim() {
        "libx264" => Some(TvEncoder::Libx264),
        "h264_mf" => Some(TvEncoder::H264Mf),
        "h264_nvenc" => Some(TvEncoder::H264Nvenc),
        "h264_qsv" => Some(TvEncoder::H264Qsv),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioIn {
    /// Already AAC. The copy keeps that stream.
    Aac,
    /// Any other codec. The copy converts it to AAC.
    Other,
    /// No audio stream. The copy gets a silent AAC track so the cast check passes.
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TvPlan {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// True when the source was over 60 fps and the copy forces 60.
    pub fps_capped: bool,
    pub tone_map: bool,
    /// Input transfer for the tone-map. Untagged 10-bit uses bt709.
    pub transfer_in: &'static str,
    pub encoder: TvEncoder,
    pub audio: AudioIn,
}

/// Size and frame rate of the copy.
///
/// A frame that already fits in 3840×2160 (either orientation) keeps its size,
/// so 3840×2160 stays 3840×2160. Anything larger is scaled until the longer
/// side is 2160, the aspect ratio is kept, and both sides are even. Frame
/// rate above 60 becomes 60. A lower rate is left alone.
pub(crate) fn plan_tv(
    width: u32,
    height: u32,
    fps: f64,
    tone_map: bool,
    audio: AudioIn,
    encoder: TvEncoder,
) -> TvPlan {
    let (width, height) = target_size(width, height);
    let fps_capped = fps.is_finite() && fps > 60.0;
    let fps = if fps_capped { 60.0 } else { fps };
    TvPlan {
        width,
        height,
        fps,
        fps_capped,
        tone_map,
        transfer_in: "bt709",
        encoder,
        audio,
    }
}

fn target_size(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (2, 2);
    }
    if fits_box(width, height, UHD_W, UHD_H) {
        return (even_down(width as u64), even_down(height as u64));
    }
    let long = u64::from(width.max(height)).max(1);
    let max_long = u64::from(MAX_LONG_SIDE);
    (
        even_down(u64::from(width) * max_long / long),
        even_down(u64::from(height) * max_long / long),
    )
}

fn even_down(n: u64) -> u32 {
    let n = n.max(2);
    let n = if n % 2 == 0 { n } else { n - 1 };
    u32::try_from(n).unwrap_or(u32::MAX) & !1
}

/// ffmpeg arguments for one TV copy, not including the executable.
/// The output path is whatever the caller passes. Callers pass a cache path.
pub(crate) fn ffmpeg_arguments(plan: &TvPlan, input: &Path, output: &Path) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        input.display().to_string(),
    ];
    if plan.audio == AudioIn::None {
        args.extend([
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "anullsrc=r=48000:cl=stereo".into(),
        ]);
    }
    args.push("-map".into());
    args.push("0:v:0".into());
    args.push("-map".into());
    args.push(if plan.audio == AudioIn::None {
        "1:a:0".into()
    } else {
        "0:a:0?".into()
    });
    args.push("-c:v".into());
    args.push(plan.encoder.as_str().to_string());
    args.extend([
        "-profile:v".into(),
        "high".into(),
        "-level:v".into(),
        "5.2".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-vf".into(),
        video_filter(plan),
        "-b:v".into(),
        "35M".into(),
        "-maxrate".into(),
        "40M".into(),
        "-bufsize".into(),
        "40M".into(),
    ]);
    match plan.audio {
        AudioIn::Aac => args.extend(["-c:a".into(), "copy".into()]),
        AudioIn::Other | AudioIn::None => {
            args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "192k".into(), "-ac".into(), "2".into()]);
        }
    }
    if plan.audio == AudioIn::None {
        args.push("-shortest".into());
    }
    args.extend(["-movflags".into(), "+faststart".into()]);
    args.push(output.display().to_string());
    args
}

fn video_filter(plan: &TvPlan) -> String {
    let mut parts = Vec::new();
    if plan.fps_capped {
        parts.push("fps=60".to_string());
    }
    if plan.tone_map {
        parts.push(tone_map_filter(plan.transfer_in));
    }
    parts.push(format!(
        "scale={}:{}:flags=lanczos",
        plan.width, plan.height
    ));
    parts.push("format=yuv420p".to_string());
    parts.join(",")
}

/// HDR (PQ, HLG, BT.2020) is tone-mapped to BT.709. 10-bit BT.709, including a
/// file that does not name a transfer, is already SDR: only the bit depth changes.
///
/// zscale on `gbrpf32le` returns "no path between colorspaces" in the pinned
/// ffmpeg, and zscale also has no path from BT.709 into linear light. The HDR
/// chain therefore runs zscale only on YUV, and the 10-bit SDR chain does not
/// use zscale at all.
fn tone_map_filter(transfer_in: &str) -> String {
    let (primaries, matrix) = match transfer_in {
        "smpte2084" | "arib-std-b67" | "bt2020-10" => ("bt2020", "bt2020nc"),
        _ => {
            return "format=yuv420p".to_string();
        }
    };
    format!(
        "zscale=transferin={transfer_in}:primariesin={primaries}:matrixin={matrix}:rangein=tv:transfer=linear:npl=100,format=gbrpf32le,tonemap=hable:desat=0,format=yuv420p10le,zscale=transferin=linear:primariesin=bt709:matrixin=bt709:rangein=tv:transfer=bt709:primaries=bt709:matrix=bt709:range=tv"
    )
}

pub(crate) enum ForCast {
    Original { derived: Option<String> },
    Copy { derived: Option<String> },
    Preparing { derived: Option<String>, reasons: String },
    Skip { derived: Option<String>, reasons: String },
}

pub(crate) enum CopyPoll {
    Ready,
    Running,
    Failed,
    Absent,
}

/// What to cast for this video. A file the TV can already play is the original.
/// Anything else starts a TV copy, or uses one that is already in the cache.
pub(crate) fn for_cast(path: &Path, item: u32) -> ForCast {
    let decision = decide_cast_video(path);
    let derived = decision.derived.clone();
    let Some(reasons) = decision.skip else {
        return ForCast::Original { derived };
    };
    match ensure_copy(path, item, &reasons) {
        Ensure::Ready => ForCast::Copy { derived },
        Ensure::Pending => ForCast::Preparing { derived, reasons },
        Ensure::GiveUp => ForCast::Skip { derived, reasons },
    }
}

enum Ensure {
    Ready,
    Pending,
    GiveUp,
}

fn canon_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn ensure_copy(path: &Path, item: u32, reasons: &str) -> Ensure {
    let owned = canon_path(path);
    let path = owned.as_path();
    let meta = match fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta,
        _ => {
            let key = cache_key(path, 0, UNIX_EPOCH);
            return fail_once(&key, item, "source unreadable");
        }
    };
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    let key = cache_key(path, meta.len(), modified);
    let cache_dir = active_cache_dir();
    let dest = cache_dir.join(format!("{key}.mp4"));

    if dest.is_file() {
        if copy_passes(&dest) {
            let _ = touch(&dest);
            remember_ready(path, &dest);
            cast_log(
                "tv_copy",
                &format!("cache_hit item={}", item_log_token(item)),
            );
            return Ensure::Ready;
        }
        let _ = fs::remove_file(&dest);
        return fail_once(&key, item, "copy failed the cast check");
    }

    let encoder = resolve_encoder();
    {
        let mut hub = hub();
        if hub.failed.contains(&key) {
            return Ensure::GiveUp;
        }
        if hub
            .jobs
            .get(&key)
            .is_some_and(|job| matches!(job.state, JobState::Running))
        {
            return Ensure::Pending;
        }
        if matches!(encoder, ActiveEncoder::Missing) {
            hub.failed.insert(key);
            drop(hub);
            log_fail(item, "ffmpeg missing");
            return Ensure::GiveUp;
        }
        hub.jobs.insert(
            key.clone(),
            JobSlot {
                state: JobState::Running,
                cache_dir: cache_dir.clone(),
            },
        );
    }

    cast_log(
        "tv_copy",
        &format!(
            "start item={} reasons={reasons}",
            item_log_token(item)
        ),
    );
    let log_dir = super::cast_log_override_dir();
    let limit = active_limit();
    let spec = JobSpec {
        key: key.clone(),
        source: path.to_path_buf(),
        dest,
        // The cast check rejects anything whose name is not .mp4 or .mov.
        // The part stays in the cache directory, never beside the original.
        part: cache_dir.join(format!("{key}.part.mp4")),
        cache_dir,
        item,
        encoder,
        limit,
        log_dir,
        started: Instant::now(),
    };
    thread::spawn(move || run_job(spec));
    Ensure::Pending
}

fn copy_passes(path: &Path) -> bool {
    super::decide_cast_video_logged(path, false).skip.is_none()
}

fn fail_once(key: &str, item: u32, err: &str) -> Ensure {
    let mut hub = hub();
    if !hub.failed.insert(key.to_string()) {
        return Ensure::GiveUp;
    }
    drop(hub);
    log_fail(item, err);
    Ensure::GiveUp
}

fn log_fail(item: u32, err: &str) {
    cast_log(
        "tv_copy",
        &format!("fail item={} err={}", item_log_token(item), safe_err(err)),
    );
}

pub(crate) fn poll_copy(path: &Path) -> CopyPoll {
    if servable_copy(path).is_some() {
        return CopyPoll::Ready;
    }
    let owned = canon_path(path);
    let Some(key) = current_key(&owned) else {
        return CopyPoll::Absent;
    };
    let hub = hub();
    if hub.failed.contains(&key) {
        return CopyPoll::Failed;
    }
    match hub.jobs.get(&key).map(|job| job.state) {
        Some(JobState::Running) => CopyPoll::Running,
        Some(JobState::Failed) | Some(JobState::Ready) => CopyPoll::Failed,
        None => CopyPoll::Absent,
    }
}

/// The cached MP4 for `path`, when it matches this file's size and mtime and
/// still passes the cast check. Never a file sitting next to the original.
pub(crate) fn servable_copy(path: &Path) -> Option<PathBuf> {
    let owned = canon_path(path);
    let path = owned.as_path();
    let Some(key) = current_key(path) else {
        return None;
    };
    let copy = {
        let hub = hub();
        hub.ready.get(path).cloned()
    }?;
    if copy.file_stem().and_then(|s| s.to_str()) != Some(key.as_str()) {
        hub().ready.remove(path);
        return None;
    }
    let parent = copy.parent()?;
    if path.parent() == Some(parent) {
        return None;
    }
    if !copy.is_file() {
        return None;
    }
    if !copy_passes(&copy) {
        return None;
    }
    Some(copy)
}

/// True when the original itself would be skipped. The media server uses this
/// so a range request cannot fall back to the file the TV already rejected.
pub(crate) fn original_needs_copy(path: &Path) -> bool {
    !copy_passes(path)
}

pub(crate) fn wait_until_settled(timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let running = hub()
            .jobs
            .values()
            .any(|job| matches!(job.state, JobState::Running));
        if !running {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

struct JobSpec {
    key: String,
    source: PathBuf,
    dest: PathBuf,
    part: PathBuf,
    cache_dir: PathBuf,
    item: u32,
    encoder: ActiveEncoder,
    limit: u64,
    log_dir: Option<PathBuf>,
    started: Instant,
}

fn run_job(spec: JobSpec) {
    super::apply_cast_log_dir(spec.log_dir.clone());
    let result = encode_job(&spec);
    // Log before the job leaves Running, so a waiter that sees it settle
    // also sees the done or fail line.
    match &result {
        Ok(bytes) => {
            let ms = spec.started.elapsed().as_millis();
            cast_log(
                "tv_copy",
                &format!(
                    "done item={} ms={ms} bytes={bytes}",
                    item_log_token(spec.item)
                ),
            );
        }
        Err(err) => {
            let _ = fs::remove_file(&spec.part);
            let _ = fs::remove_file(&spec.dest);
            log_fail(spec.item, err);
        }
    }
    let mut hub = hub();
    match result {
        Ok(_) => {
            hub.ready.insert(spec.source.clone(), spec.dest.clone());
            if let Some(job) = hub.jobs.get_mut(&spec.key) {
                job.state = JobState::Ready;
            }
        }
        Err(_) => {
            hub.failed.insert(spec.key.clone());
            hub.ready.remove(&spec.source);
            if let Some(job) = hub.jobs.get_mut(&spec.key) {
                job.state = JobState::Failed;
            }
        }
    }
}

fn encode_job(spec: &JobSpec) -> Result<u64, String> {
    fs::create_dir_all(&spec.cache_dir).map_err(|_| "could not create the cache".to_string())?;
    if spec.part.parent() != Some(spec.cache_dir.as_path())
        || spec.dest.parent() != Some(spec.cache_dir.as_path())
    {
        return Err("refusing to write next to the original".into());
    }
    match &spec.encoder {
        ActiveEncoder::Missing => return Err("ffmpeg missing".into()),
        ActiveEncoder::Fail(err) => {
            if !spec.encoder_delay().is_zero() {
                thread::sleep(spec.encoder_delay());
            }
            return Err(err.clone());
        }
        ActiveEncoder::Write { bytes, delay } => {
            if !delay.is_zero() {
                thread::sleep(*delay);
            }
            fs::write(&spec.part, bytes).map_err(|_| "could not write the copy".to_string())?;
        }
        ActiveEncoder::Ffmpeg(ffmpeg) => {
            run_ffmpeg(ffmpeg, &spec.source, &spec.part, configured_encoder())?;
        }
    }
    if !copy_passes(&spec.part) {
        let _ = fs::remove_file(&spec.part);
        return Err("copy failed the cast check".into());
    }
    let bytes = fs::metadata(&spec.part)
        .map(|m| m.len())
        .unwrap_or(0);
    if bytes > spec.limit {
        let _ = fs::remove_file(&spec.part);
        return Err("cache limit".into());
    }
    evict_to_fit(&spec.cache_dir, spec.limit, bytes, Some(spec.dest.as_path()));
    fs::rename(&spec.part, &spec.dest).map_err(|_| "could not store the copy".to_string())?;
    Ok(bytes)
}

impl JobSpec {
    fn encoder_delay(&self) -> Duration {
        match &self.encoder {
            ActiveEncoder::Fail(_) => Duration::from_millis(0),
            ActiveEncoder::Write { delay, .. } => *delay,
            _ => Duration::from_millis(0),
        }
    }
}

fn run_ffmpeg(ffmpeg: &Path, input: &Path, output: &Path, encoder: TvEncoder) -> Result<(), String> {
    let probed = probe_source(ffmpeg, input)?;
    let mut plan = plan_tv(
        probed.width,
        probed.height,
        probed.fps,
        probed.hdr_or_10bit,
        probed.audio,
        encoder,
    );
    plan.transfer_in = probed.transfer_in;
    let args = ffmpeg_arguments(&plan, input, output);
    let mut cmd = Command::new(ffmpeg);
    cmd.args(&args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|_| "could not start ffmpeg".to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let code = out.status.code().unwrap_or(1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!("exit {code} {}", safe_err(&stderr)))
}

struct Probe {
    width: u32,
    height: u32,
    fps: f64,
    hdr_or_10bit: bool,
    transfer_in: &'static str,
    audio: AudioIn,
}

fn probe_source(ffmpeg: &Path, input: &Path) -> Result<Probe, String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-i"])
        .arg(input)
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|_| "could not start ffmpeg".to_string())?;
    let text = String::from_utf8_lossy(&out.stderr);
    parse_probe(&text).ok_or_else(|| "could not read the video".to_string())
}

fn parse_probe(text: &str) -> Option<Probe> {
    let mut video: Option<(u32, u32, f64, bool, &'static str)> = None;
    let mut audio: Option<AudioIn> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if video.is_none() && trimmed.contains("Video:") {
            video = parse_video_line(trimmed);
        } else if audio.is_none() && trimmed.contains("Audio:") {
            audio = Some(if trimmed.to_ascii_lowercase().contains("aac") {
                AudioIn::Aac
            } else {
                AudioIn::Other
            });
        }
    }
    let (width, height, fps, hdr_or_10bit, transfer_in) = video?;
    Some(Probe {
        width,
        height,
        fps,
        hdr_or_10bit,
        transfer_in,
        audio: audio.unwrap_or(AudioIn::None),
    })
}

fn parse_video_line(line: &str) -> Option<(u32, u32, f64, bool, &'static str)> {
    let (width, height) = first_dimensions(line)?;
    let fps = first_fps(line).unwrap_or(0.0);
    let lower = line.to_ascii_lowercase();
    let ten_bit = lower.contains("p10")
        || lower.contains("p12")
        || lower.contains("10le")
        || lower.contains("10be")
        || lower.contains("12le")
        || lower.contains("p010");
    let transfer_in = if lower.contains("smpte2084") {
        "smpte2084"
    } else if lower.contains("arib-std-b67") {
        "arib-std-b67"
    } else if lower.contains("bt2020") {
        "bt2020-10"
    } else {
        "bt709"
    };
    let hdr = transfer_in != "bt709";
    Some((width, height, fps, ten_bit || hdr, transfer_in))
}

fn first_dimensions(line: &str) -> Option<(u32, u32)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'x' {
                let w: u32 = line[start..i].parse().ok()?;
                let h_start = i + 1;
                let mut h_end = h_start;
                while h_end < bytes.len() && bytes[h_end].is_ascii_digit() {
                    h_end += 1;
                }
                if h_end > h_start {
                    if let Ok(h) = line[h_start..h_end].parse::<u32>() {
                        if w >= 2 && h >= 2 {
                            return Some((w, h));
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn first_fps(line: &str) -> Option<f64> {
    let marker = " fps";
    let at = line.find(marker)?;
    let head = &line[..at];
    let start = head
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let fps: f64 = head[start..].parse().ok()?;
    if fps.is_finite() && fps > 0.0 {
        Some(fps)
    } else {
        None
    }
}

/// Encode `input` to `output` with the pinned ffmpeg. Used by the end-to-end test.
pub(crate) fn encode_with_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    encoder: TvEncoder,
) -> Result<(), String> {
    run_ffmpeg(ffmpeg, input, output, encoder)
}

pub(crate) fn bundled_ffmpeg() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// The pinned Windows build, when this checkout has already verified it.
pub(crate) fn pinned_ffmpeg_exe() -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("ffmpeg")
        .join("ffmpeg.exe");
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

pub(crate) fn configured_encoder() -> TvEncoder {
    if let Some(encoder) = TEST_ENCODER_SETTING.with(|slot| *slot.borrow()) {
        return encoder;
    }
    let path = encoder_file();
    let Ok(text) = fs::read_to_string(&path) else {
        return TvEncoder::Libx264;
    };
    parse_encoder(text.trim()).unwrap_or(TvEncoder::Libx264)
}

pub(crate) fn encoder_name() -> String {
    configured_encoder().as_str().to_string()
}

pub(crate) fn set_encoder_name(name: &str) -> Result<(), String> {
    let encoder = parse_encoder(name).ok_or_else(|| {
        "TV encoder must be libx264, h264_mf, h264_nvenc, or h264_qsv.".to_string()
    })?;
    let path = encoder_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "Could not store the TV encoder setting.".to_string())?;
    }
    fs::write(&path, format!("{}\n", encoder.as_str()))
        .map_err(|_| "Could not store the TV encoder setting.".to_string())?;
    Ok(())
}

fn encoder_file() -> PathBuf {
    if let Some(path) = TEST_ENCODER_FILE.with(|slot| slot.borrow().clone()) {
        return path;
    }
    app_data_dir().join("tv-encoder")
}

fn app_data_dir() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) => PathBuf::from(local).join("com.johnalindogan.slideshowpro"),
        None => std::env::temp_dir().join("com.johnalindogan.slideshowpro"),
    }
}

fn default_cache_dir() -> PathBuf {
    app_data_dir().join("tv-cache")
}

fn active_cache_dir() -> PathBuf {
    TEST_CACHE.with(|slot| slot.borrow().clone()).unwrap_or_else(default_cache_dir)
}

fn active_limit() -> u64 {
    TEST_LIMIT.with(|slot| slot.borrow().unwrap_or(CACHE_LIMIT_DEFAULT))
}

fn current_key(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    Some(cache_key(path, meta.len(), modified))
}

/// Stable id of one source file. Path, size, and mtime all change the key.
pub(crate) fn cache_key(path: &Path, len: u64, modified: SystemTime) -> String {
    let (secs, nanos) = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0));
    let mut raw = path.to_string_lossy().as_bytes().to_vec();
    raw.push(0);
    raw.extend_from_slice(&len.to_le_bytes());
    raw.push(0);
    raw.extend_from_slice(&secs.to_le_bytes());
    raw.extend_from_slice(&nanos.to_le_bytes());
    let hi = fnv1a64(&raw, 0xcbf2_9ce4_8422_2325);
    let lo = fnv1a64(&raw, 0x6c62_272e_07bb_0142);
    format!("{hi:016x}{lo:016x}")
}

fn fnv1a64(data: &[u8], offset: u64) -> u64 {
    let mut hash = offset;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub(crate) fn cache_output(cache_dir: &Path, source: &Path, len: u64, modified: SystemTime) -> PathBuf {
    cache_dir.join(format!("{}.mp4", cache_key(source, len, modified)))
}

fn remember_ready(source: &Path, dest: &Path) {
    hub().ready.insert(source.to_path_buf(), dest.to_path_buf());
}

fn touch(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_modified(SystemTime::now())
}

/// Delete least-recently-used copies until `incoming` bytes fit under `limit`.
pub(crate) fn evict_to_fit(cache_dir: &Path, limit: u64, incoming: u64, keep: Option<&Path>) {
    let mut files = cache_mp4s(cache_dir);
    files.retain(|entry| keep != Some(entry.path.as_path()));
    files.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.path.cmp(&b.path)));
    let mut used: u64 = files.iter().map(|entry| entry.bytes).sum();
    for entry in files {
        if used.saturating_add(incoming) <= limit {
            break;
        }
        if fs::remove_file(&entry.path).is_ok() {
            used = used.saturating_sub(entry.bytes);
            cast_log("tv_copy", &format!("evict bytes={}", entry.bytes));
        }
    }
}

struct CacheFile {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

fn cache_mp4s(cache_dir: &Path) -> Vec<CacheFile> {
    let Ok(read) = fs::read_dir(cache_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("mp4") {
            continue;
        }
        // An in-progress encode is `{key}.part.mp4` so the cast check can read it.
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".part.mp4"))
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        out.push(CacheFile {
            path,
            bytes: meta.len(),
            modified: meta.modified().unwrap_or(UNIX_EPOCH),
        });
    }
    out
}

fn safe_err(err: &str) -> String {
    let mut out = String::new();
    for ch in err.chars() {
        if ch == '\\' || ch == '/' || ch == '\n' || ch == '\r' || ch == '\t' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }
        out.push(ch);
        if out.len() >= 160 {
            break;
        }
    }
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        "encode failed".to_string()
    } else {
        flat
    }
}

#[derive(Clone)]
enum ActiveEncoder {
    Missing,
    Fail(String),
    Write { bytes: Vec<u8>, delay: Duration },
    Ffmpeg(PathBuf),
}

#[derive(Clone)]
pub(crate) enum TestEncoder {
    Missing,
    Fail(String),
    Write { bytes: Vec<u8>, delay: Duration },
}

fn resolve_encoder() -> ActiveEncoder {
    if let Some(test) = TEST_ENCODER.with(|slot| slot.borrow().clone()) {
        return match test {
            TestEncoder::Missing => ActiveEncoder::Missing,
            TestEncoder::Fail(err) => ActiveEncoder::Fail(err),
            TestEncoder::Write { bytes, delay } => ActiveEncoder::Write { bytes, delay },
        };
    }
    match bundled_ffmpeg() {
        Some(path) => ActiveEncoder::Ffmpeg(path),
        None => ActiveEncoder::Missing,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobState {
    Running,
    Ready,
    Failed,
}

struct JobSlot {
    state: JobState,
    cache_dir: PathBuf,
}

struct Hub {
    jobs: HashMap<String, JobSlot>,
    ready: HashMap<PathBuf, PathBuf>,
    failed: HashSet<String>,
}

fn hub() -> std::sync::MutexGuard<'static, Hub> {
    HUB.get_or_init(|| {
        Mutex::new(Hub {
            jobs: HashMap::new(),
            ready: HashMap::new(),
            failed: HashSet::new(),
        })
    })
    .lock()
    .unwrap_or_else(|err| err.into_inner())
}

static HUB: std::sync::OnceLock<Mutex<Hub>> = std::sync::OnceLock::new();

thread_local! {
    static TEST_ENCODER: std::cell::RefCell<Option<TestEncoder>> = const { std::cell::RefCell::new(None) };
    static TEST_CACHE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static TEST_LIMIT: std::cell::RefCell<Option<u64>> = const { std::cell::RefCell::new(None) };
    static TEST_ENCODER_FILE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static TEST_ENCODER_SETTING: std::cell::RefCell<Option<TvEncoder>> = const { std::cell::RefCell::new(None) };
}

pub(crate) struct TestScope {
    cache: PathBuf,
}

impl TestScope {
    pub(crate) fn new(cache: PathBuf, encoder: TestEncoder) -> Self {
        let _ = fs::create_dir_all(&cache);
        TEST_CACHE.with(|slot| *slot.borrow_mut() = Some(cache.clone()));
        TEST_ENCODER.with(|slot| *slot.borrow_mut() = Some(encoder));
        Self { cache }
    }
}

impl Drop for TestScope {
    fn drop(&mut self) {
        TEST_CACHE.with(|slot| *slot.borrow_mut() = None);
        TEST_ENCODER.with(|slot| *slot.borrow_mut() = None);
        let mut hub = hub();
        hub.jobs.retain(|_, job| job.cache_dir != self.cache);
        hub.ready.retain(|_, dest| dest.parent() != Some(self.cache.as_path()));
    }
}

pub(crate) fn set_test_encoder_file(path: Option<PathBuf>) {
    TEST_ENCODER_FILE.with(|slot| *slot.borrow_mut() = path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_value(args: &[String], flag: &str) -> String {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .unwrap_or_default()
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|pair| pair[0] == flag && pair[1] == value)
    }

    fn request(w: u32, h: u32, fps: f64, encoder: TvEncoder) -> Vec<String> {
        let plan = plan_tv(w, h, fps, false, AudioIn::Aac, encoder);
        ffmpeg_arguments(&plan, Path::new("in.mkv"), Path::new("out.mp4"))
    }

    #[test]
    fn cache_key_changes_with_path_size_or_mtime() {
        let when = UNIX_EPOCH + Duration::from_secs(10);
        let later = UNIX_EPOCH + Duration::from_secs(11);
        let path_a = Path::new(r"C:\slides\a.mkv");
        let path_b = Path::new(r"C:\slides\b.mkv");
        let key = cache_key(path_a, 100, when);
        assert_eq!(key, cache_key(path_a, 100, when));
        assert_ne!(key, cache_key(path_b, 100, when));
        assert_ne!(key, cache_key(path_a, 101, when));
        assert_ne!(key, cache_key(path_a, 100, later));
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn cache_file_stays_in_the_cache_and_not_beside_the_original() {
        let source = Path::new(r"D:\slides\clip.mkv");
        let cache = Path::new(r"C:\Users\me\AppData\Local\com.johnalindogan.slideshowpro\tv-cache");
        let out = cache_output(cache, source, 20, UNIX_EPOCH);
        assert_eq!(out.parent(), Some(cache));
        assert_ne!(out.parent(), source.parent());
        assert_eq!(out.extension().and_then(|ext| ext.to_str()), Some("mp4"));
    }

    #[test]
    fn cache_evicts_the_least_recently_used_copy_first() {
        let root = std::env::temp_dir().join(format!("slidex-tv-cache-{}", uuid_tail()));
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let _guard = super::super::push_cast_log_dir(logs.clone());
        let old = root.join("old.mp4");
        let mid = root.join("mid.mp4");
        let new = root.join("new.mp4");
        write_sized(&old, 300, UNIX_EPOCH + Duration::from_secs(1));
        write_sized(&mid, 300, UNIX_EPOCH + Duration::from_secs(2));
        write_sized(&new, 300, UNIX_EPOCH + Duration::from_secs(3));
        evict_to_fit(&root, 1000, 400, None);
        assert!(!old.exists(), "the oldest copy is deleted first");
        assert!(mid.exists());
        assert!(new.exists());
        let written = fs::read_to_string(logs.join("cast.log")).unwrap_or_default();
        assert!(
            written.contains("tv_copy evict bytes=300"),
            "{written}"
        );
        assert!(!written.contains("old.mp4"), "{written}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ffmpeg_args_scale_square_keep_uhd_and_portrait() {
        let square = request(2560, 2560, 90.0, TvEncoder::Libx264);
        let square_vf = arg_value(&square, "-vf");
        assert!(square_vf.contains("fps=60"), "{square_vf}");
        assert!(square_vf.contains("scale=2160:2160"), "{square_vf}");
        assert!(square_vf.contains("format=yuv420p"), "{square_vf}");
        assert!(!square_vf.contains("tonemap"), "{square_vf}");
        assert!(has_pair(&square, "-c:v", "libx264"));
        assert!(has_pair(&square, "-profile:v", "high"));
        assert!(has_pair(&square, "-level:v", "5.2"));
        assert!(has_pair(&square, "-pix_fmt", "yuv420p"));
        assert!(has_pair(&square, "-b:v", "35M"));
        assert!(has_pair(&square, "-maxrate", "40M"));
        assert!(has_pair(&square, "-bufsize", "40M"));
        assert!(has_pair(&square, "-movflags", "+faststart"));
        assert!(has_pair(&square, "-c:a", "copy"));

        let uhd = request(3840, 2160, 30.0, TvEncoder::Libx264);
        let uhd_vf = arg_value(&uhd, "-vf");
        assert!(uhd_vf.contains("scale=3840:2160"), "{uhd_vf}");
        assert!(!uhd_vf.contains("fps="), "{uhd_vf}");

        let portrait = request(2160, 3840, 24.0, TvEncoder::Libx264);
        let portrait_vf = arg_value(&portrait, "-vf");
        assert!(portrait_vf.contains("scale=2160:3840"), "{portrait_vf}");
        assert!(!portrait_vf.contains("fps="), "{portrait_vf}");

        let phone = request(1080, 1920, 30.0, TvEncoder::Libx264);
        assert!(arg_value(&phone, "-vf").contains("scale=1080:1920"));

        let tall = request(2304, 4096, 50.0, TvEncoder::Libx264);
        let tall_vf = arg_value(&tall, "-vf");
        assert!(tall_vf.contains("scale=1214:2160"), "{tall_vf}");
        assert!(!tall_vf.contains("fps="), "{tall_vf}");

        let plan = plan_tv(2560, 2560, 90.0, false, AudioIn::Aac, TvEncoder::Libx264);
        assert_eq!((plan.width, plan.height), (2160, 2160));
        assert_eq!(plan.fps, 60.0);
        let kept = plan_tv(3840, 2160, 30.0, false, AudioIn::Aac, TvEncoder::Libx264);
        assert_eq!((kept.width, kept.height), (3840, 2160));
        assert_eq!(kept.fps, 30.0);
    }

    #[test]
    fn encoder_setting_is_one_of_four_and_hdr_is_tone_mapped() {
        assert_eq!(parse_encoder("libx264"), Some(TvEncoder::Libx264));
        assert_eq!(parse_encoder("h264_mf"), Some(TvEncoder::H264Mf));
        assert_eq!(parse_encoder("h264_nvenc"), Some(TvEncoder::H264Nvenc));
        assert_eq!(parse_encoder("h264_qsv"), Some(TvEncoder::H264Qsv));
        assert!(parse_encoder("hevc_nvenc").is_none());
        assert_eq!(TvEncoder::Libx264.as_str(), "libx264");

        for encoder in [TvEncoder::H264Mf, TvEncoder::H264Nvenc, TvEncoder::H264Qsv] {
            let args = request(1280, 720, 30.0, encoder);
            assert!(has_pair(&args, "-c:v", encoder.as_str()), "{args:?}");
        }

        let ten_bit = plan_tv(1920, 1080, 24.0, true, AudioIn::Other, TvEncoder::H264Nvenc);
        let ten_bit_vf = arg_value(
            &ffmpeg_arguments(&ten_bit, Path::new("in.mov"), Path::new("out.mp4")),
            "-vf",
        );
        assert!(!ten_bit_vf.contains("tonemap"), "{ten_bit_vf}");
        assert!(!ten_bit_vf.contains("zscale"), "{ten_bit_vf}");
        assert!(ten_bit_vf.contains("format=yuv420p"), "{ten_bit_vf}");

        let mut hdr = ten_bit;
        hdr.transfer_in = "smpte2084";
        let args = ffmpeg_arguments(&hdr, Path::new("in.mov"), Path::new("out.mp4"));
        let vf = arg_value(&args, "-vf");
        assert!(vf.contains("tonemap=hable"), "{vf}");
        assert!(vf.contains("format=gbrpf32le,tonemap=hable"), "{vf}");
        assert!(vf.contains("transferin=smpte2084"), "{vf}");
        assert!(
            vf.contains("format=yuv420p10le,zscale=transferin=linear"),
            "{vf}"
        );
        assert!(vf.contains("format=yuv420p"), "{vf}");
        assert!(has_pair(&args, "-c:v", "h264_nvenc"));
        assert!(has_pair(&args, "-c:a", "aac"));

        let silent = plan_tv(640, 360, 30.0, false, AudioIn::None, TvEncoder::Libx264);
        let args = ffmpeg_arguments(&silent, Path::new("in.mkv"), Path::new("out.mp4"));
        assert!(args.iter().any(|arg| arg.contains("anullsrc")));
        assert!(has_pair(&args, "-c:a", "aac"));
        assert!(args.iter().any(|arg| arg == "-shortest"));
    }

    #[test]
    fn encoder_setting_file_defaults_to_libx264() {
        let root = std::env::temp_dir().join(format!("slidex-tv-encoder-{}", uuid_tail()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("tv-encoder");
        set_test_encoder_file(Some(file.clone()));
        assert_eq!(configured_encoder(), TvEncoder::Libx264);
        set_encoder_name("h264_qsv").unwrap();
        assert_eq!(configured_encoder(), TvEncoder::H264Qsv);
        assert!(set_encoder_name("libx265").is_err());
        assert_eq!(configured_encoder(), TvEncoder::H264Qsv);
        set_test_encoder_file(None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_line_reads_size_fps_and_hdr() {
        let line = "Stream #0:0: Video: hevc (Main 10), yuv420p10le(tv, bt2020nc/bt2020/smpte2084), 3840x2160, 90 fps, 90 tbr";
        let (w, h, fps, hdr, transfer) = parse_video_line(line).unwrap();
        assert_eq!((w, h), (3840, 2160));
        assert_eq!(fps, 90.0);
        assert!(hdr);
        assert_eq!(transfer, "smpte2084");
        let sdr = "Stream #0:0: Video: h264 (High), yuv420p(tv, bt709), 1920x1080 [SAR 1:1 DAR 16:9], 30 fps, 30 tbr";
        let (w, h, fps, hdr, transfer) = parse_video_line(sdr).unwrap();
        assert_eq!((w, h, hdr), (1920, 1080, false));
        assert_eq!(fps, 30.0);
        assert_eq!(transfer, "bt709");
    }

    #[test]
    fn synthetic_clip_passes_the_cast_check_when_ffmpeg_is_present() {
        let require = std::env::var("SLIDEX_REQUIRE_FFMPEG").ok().as_deref() == Some("1");
        let Some(ffmpeg) = pinned_ffmpeg_exe() else {
            if require {
                panic!("pinned ffmpeg.exe is missing");
            }
            eprintln!("pinned ffmpeg.exe is not in the checkout; skipping the synthetic encode");
            return;
        };
        match Command::new(&ffmpeg).arg("-version").output() {
            Ok(out) if out.status.success() => {}
            other => {
                if require {
                    panic!("pinned ffmpeg.exe did not run: {other:?}");
                }
                eprintln!("pinned ffmpeg.exe is not runnable here; skipping the synthetic encode");
                return;
            }
        }
        let root = std::env::temp_dir().join(format!("slidex-tv-synth-{}", uuid_tail()));
        fs::create_dir_all(&root).unwrap();
        let src = root.join("src.mp4");
        let out = root.join("tv.mp4");
        let made = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=90:duration=0.4",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=0.4",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p10le",
                "-profile:v",
                "high10",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&src)
            .status();
        assert!(made.unwrap().success(), "could not write the synthetic source");
        assert!(
            super::super::decide_cast_video(&src).skip.is_some(),
            "the 90 fps 10-bit source is not cast as-is"
        );
        encode_with_ffmpeg(&ffmpeg, &src, &out, TvEncoder::Libx264).unwrap();
        let decision = super::super::decide_cast_video(&out);
        assert!(
            decision.skip.is_none(),
            "the TV copy must pass the cast check: {:?}",
            decision.skip
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn write_sized(path: &Path, bytes: u64, modified: SystemTime) {
        let file = File::create(path).unwrap();
        file.set_len(bytes).unwrap();
        file.set_modified(modified).unwrap();
    }

    fn uuid_tail() -> String {
        format!(
            "{:x}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
    }
}
