//! Background H.264 copy for a video the TV will not play as-is.
//!
//! The bundled ffmpeg lives beside the app, never on PATH. The video encoder
//! is one setting (`libx264` by default). `h264_mf`, `h264_nvenc`, and
//! `h264_qsv` are the alternatives, so a later LGPL build can switch encoders
//! without another code path.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{cast_log, decide_cast_video, fits_box, item_log_token};

/// Shown on the cast banner while a TV copy is being built.
pub(crate) const PREPARING_TV: &str = "Preparing for TV…";

/// Copies live under `%LOCALAPPDATA%\com.johnalindogan.slideshowpro\tv-cache`.
const CACHE_LIMIT_DEFAULT: u64 = 20 * 1024 * 1024 * 1024;
/// One TV cache, so this is also the process-wide cap. Further encodes wait
/// in a FIFO queue instead of each calling `thread::spawn`.
const MAX_CONCURRENT_ENCODES: usize = 2;
/// First retry waits this long. Each later failure doubles it, up to 64×.
const RETRY_BACKOFF: Duration = Duration::from_secs(5);
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
            args.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                "192k".into(),
                "-ac".into(),
                "2".into(),
            ]);
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
/// use zscale at all. `setparams` stamps primaries and matrix first: the pinned
/// zscale returns the same error when those tags are missing and `primariesin`
/// names BT.2020 anyway.
fn tone_map_filter(transfer_in: &str) -> String {
    let (primaries, matrix) = match transfer_in {
        "smpte2084" | "arib-std-b67" | "bt2020-10" => ("bt2020", "bt2020nc"),
        _ => {
            return "format=yuv420p".to_string();
        }
    };
    format!(
        "setparams=color_primaries={primaries}:color_trc={transfer_in}:colorspace={matrix},zscale=transferin={transfer_in}:primariesin={primaries}:matrixin={matrix}:rangein=tv:transfer=linear:npl=100,format=gbrpf32le,tonemap=hable:desat=0,format=yuv420p10le,zscale=transferin=linear:primariesin=bt709:matrixin=bt709:rangein=tv:transfer=bt709:primaries=bt709:matrix=bt709:range=tv"
    )
}

pub(crate) enum ForCast {
    Original {
        derived: Option<String>,
    },
    Copy {
        derived: Option<String>,
    },
    Preparing {
        derived: Option<String>,
        reasons: String,
    },
    Skip {
        derived: Option<String>,
        reasons: String,
    },
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
    let retry_unit = retry_unit();
    let part = cache_dir.join(format!("{key}.part.mp4"));
    {
        let mut hub = hub();
        if let Some(failure) = hub.failed.get(&key).cloned() {
            if Instant::now() < failure.retry_at {
                return Ensure::GiveUp;
            }
            hub.failed.remove(&key);
        }
        if hub
            .jobs
            .get(&key)
            .is_some_and(|job| matches!(job.state, JobState::Running | JobState::Queued))
        {
            return Ensure::Pending;
        }
        if matches!(encoder, ActiveEncoder::Missing) {
            note_failure(&mut hub, key, retry_unit);
            drop(hub);
            log_fail(item, "ffmpeg missing");
            return Ensure::GiveUp;
        }
        hub.jobs.insert(
            key.clone(),
            JobSlot {
                state: JobState::Queued,
                cache_dir: cache_dir.clone(),
                part: part.clone(),
                dest: dest.clone(),
            },
        );
    }

    cast_log(
        "tv_copy",
        &format!("start item={} reasons={reasons}", item_log_token(item)),
    );
    let log_dir = super::cast_log_override_dir();
    let limit = active_limit();
    let spec = JobSpec {
        key: key.clone(),
        source: path.to_path_buf(),
        dest,
        // The cast check rejects anything whose name is not .mp4 or .mov.
        // The part stays in the cache directory, never beside the original.
        part,
        cache_dir,
        item,
        encoder,
        limit,
        log_dir,
        started: Instant::now(),
        retry_unit,
    };
    enqueue_or_start(spec);
    Ensure::Pending
}

/// Start `spec` now when fewer than two encodes are running. Otherwise append
/// it to the FIFO queue. The slot was already inserted as `Queued`.
fn enqueue_or_start(spec: JobSpec) {
    let mut hub = hub();
    if running_count(&hub) < MAX_CONCURRENT_ENCODES {
        if let Some(job) = hub.jobs.get_mut(&spec.key) {
            job.state = JobState::Running;
        }
        drop(hub);
        thread::spawn(move || run_job(spec));
        return;
    }
    hub.queue.push_back(spec);
}

fn running_count(hub: &Hub) -> usize {
    hub.jobs
        .values()
        .filter(|job| job.state == JobState::Running)
        .count()
}

/// Pop the oldest queued job whose slot is still waiting, and mark it running.
fn take_next(hub: &mut Hub) -> Option<JobSpec> {
    if running_count(hub) >= MAX_CONCURRENT_ENCODES {
        return None;
    }
    while let Some(spec) = hub.queue.pop_front() {
        let still_queued = hub
            .jobs
            .get(&spec.key)
            .is_some_and(|job| job.state == JobState::Queued);
        if !still_queued {
            continue;
        }
        if let Some(job) = hub.jobs.get_mut(&spec.key) {
            job.state = JobState::Running;
        }
        return Some(spec);
    }
    None
}

fn copy_passes(path: &Path) -> bool {
    let Some(key) = current_key(path) else {
        return uncached_cast_passes(path);
    };
    if let Some(hit) = hub().checks.get(&key).copied() {
        return hit;
    }
    let passes = uncached_cast_passes(path);
    hub().checks.insert(key, passes);
    passes
}

fn uncached_cast_passes(path: &Path) -> bool {
    if COUNT_CAST_CHECKS.with(|flag| flag.get()) {
        CAST_CHECK_RUNS.with(|n| n.set(n.get().saturating_add(1)));
    }
    super::decide_cast_video_logged(path, false).skip.is_none()
}

fn fail_once(key: &str, item: u32, err: &str) -> Ensure {
    let mut hub = hub();
    if hub
        .failed
        .get(key)
        .is_some_and(|failure| Instant::now() < failure.retry_at)
    {
        return Ensure::GiveUp;
    }
    note_failure(&mut hub, key.to_string(), retry_unit());
    drop(hub);
    log_fail(item, err);
    Ensure::GiveUp
}

fn retry_unit() -> Duration {
    TEST_BACKOFF.with(|slot| slot.borrow().unwrap_or(RETRY_BACKOFF))
}

fn note_failure(hub: &mut Hub, key: String, unit: Duration) {
    let attempts = hub
        .failed
        .get(&key)
        .map(|failure| failure.attempts)
        .unwrap_or(0)
        .saturating_add(1);
    let shift = attempts.saturating_sub(1).min(6);
    let wait = unit.saturating_mul(1_u32 << shift);
    hub.failed.insert(
        key,
        Failure {
            attempts,
            retry_at: Instant::now() + wait,
        },
    );
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
    if hub.failed.contains_key(&key) {
        return CopyPoll::Failed;
    }
    match hub.jobs.get(&key).map(|job| job.state) {
        Some(JobState::Running) | Some(JobState::Queued) => CopyPoll::Running,
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
        let busy = hub()
            .jobs
            .values()
            .any(|job| matches!(job.state, JobState::Running | JobState::Queued));
        if !busy {
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
    retry_unit: Duration,
}

fn run_job(mut spec: JobSpec) {
    spec.started = Instant::now();
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
            hub.failed.remove(&spec.key);
            if let Some(job) = hub.jobs.get_mut(&spec.key) {
                job.state = JobState::Ready;
            }
        }
        Err(_) => {
            note_failure(&mut hub, spec.key.clone(), spec.retry_unit);
            hub.ready.remove(&spec.source);
            if let Some(job) = hub.jobs.get_mut(&spec.key) {
                job.state = JobState::Failed;
            }
        }
    }
    if let Some(next) = take_next(&mut hub) {
        drop(hub);
        thread::spawn(move || run_job(next));
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
        ActiveEncoder::Hold { bytes, gate } => {
            gate.wait_turn();
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
    let bytes = fs::metadata(&spec.part).map(|m| m.len()).unwrap_or(0);
    if bytes > spec.limit {
        let _ = fs::remove_file(&spec.part);
        return Err("cache limit".into());
    }
    evict_to_fit(
        &spec.cache_dir,
        spec.limit,
        bytes,
        Some(spec.dest.as_path()),
    );
    fs::rename(&spec.part, &spec.dest).map_err(|_| "could not store the copy".to_string())?;
    Ok(bytes)
}

impl JobSpec {
    fn encoder_delay(&self) -> Duration {
        match &self.encoder {
            ActiveEncoder::Fail(_) | ActiveEncoder::Hold { .. } => Duration::from_millis(0),
            ActiveEncoder::Write { delay, .. } => *delay,
            _ => Duration::from_millis(0),
        }
    }
}

fn run_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    encoder: TvEncoder,
) -> Result<(), String> {
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
    let child = spawn_tracked_ffmpeg(ffmpeg, &args)?;
    let out = wait_tracked(child)?;
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
    let args = vec![
        "-hide_banner".to_string(),
        "-i".to_string(),
        input.display().to_string(),
    ];
    let child = spawn_tracked_ffmpeg(ffmpeg, &args)?;
    let out = wait_tracked(child)?;
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
    let transfer_in = transfer_from_video_line(&lower);
    let hdr = transfer_in != "bt709";
    Some((width, height, fps, ten_bit || hdr, transfer_in))
}

/// The ffmpeg video line is `pixfmt(range, matrix/primaries/transfer, ...)`.
/// A BT.2020 matrix with an unnamed transfer is not HDR. PQ, HLG, and an
/// explicit BT.2020 transfer are.
fn transfer_from_video_line(lower: &str) -> &'static str {
    let mut rest = lower;
    while let Some(start) = rest.find('(') {
        let after = &rest[start + 1..];
        let Some(end) = after.find(')') else {
            break;
        };
        if let Some(transfer) = transfer_in_paren(&after[..end]) {
            return transfer;
        }
        rest = &after[end + 1..];
    }
    classify_transfer(lower)
}

fn transfer_in_paren(inside: &str) -> Option<&'static str> {
    let comma = inside.find(',')?;
    let spec = inside[comma + 1..].split(',').next()?.trim();
    if spec.is_empty() {
        return None;
    }
    let mut parts = spec.split('/');
    let first = parts.next()?.trim();
    let transfer = match (parts.next(), parts.next()) {
        (Some(_primaries), Some(transfer)) => transfer.trim(),
        _ => first,
    };
    Some(classify_transfer(transfer))
}

fn classify_transfer(token: &str) -> &'static str {
    if token.contains("smpte2084") {
        "smpte2084"
    } else if token.contains("arib-std-b67") {
        "arib-std-b67"
    } else if token == "bt2020" || token.starts_with("bt2020-") {
        "bt2020-10"
    } else {
        "bt709"
    }
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
    let candidate = dir.join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
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
        fs::create_dir_all(parent)
            .map_err(|_| "Could not store the TV encoder setting.".to_string())?;
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
    TEST_CACHE
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(default_cache_dir)
}

fn active_limit() -> u64 {
    if let Some(limit) = TEST_LIMIT.with(|slot| *slot.borrow()) {
        return limit;
    }
    if let Ok(text) = std::env::var("SLIDEX_TV_CACHE_LIMIT") {
        if let Some(limit) = parse_cache_limit(&text) {
            return limit;
        }
    }
    if let Ok(text) = fs::read_to_string(limit_file()) {
        if let Some(limit) = parse_cache_limit(text.trim()) {
            return limit;
        }
    }
    CACHE_LIMIT_DEFAULT
}

fn limit_file() -> PathBuf {
    if let Some(path) = TEST_LIMIT_FILE.with(|slot| slot.borrow().clone()) {
        return path;
    }
    app_data_dir().join("tv-cache-limit")
}

/// Byte count, or a number with a `KB` / `MB` / `GB` suffix (1024-based).
pub(crate) fn parse_cache_limit(text: &str) -> Option<u64> {
    let compact = text.trim().to_ascii_lowercase().replace(' ', "");
    if compact.is_empty() {
        return None;
    }
    let (number, mul) = split_limit_suffix(&compact);
    let number = number.parse::<u64>().ok()?;
    number.checked_mul(mul)
}

fn split_limit_suffix(compact: &str) -> (&str, u64) {
    const SUFFIXES: &[(&str, u64)] = &[
        ("gib", 1024 * 1024 * 1024),
        ("gb", 1024 * 1024 * 1024),
        ("mib", 1024 * 1024),
        ("mb", 1024 * 1024),
        ("kib", 1024),
        ("kb", 1024),
        ("g", 1024 * 1024 * 1024),
        ("m", 1024 * 1024),
        ("k", 1024),
    ];
    for (suffix, mul) in SUFFIXES {
        if let Some(rest) = compact.strip_suffix(suffix) {
            return (rest, *mul);
        }
    }
    (compact, 1)
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

pub(crate) fn cache_output(
    cache_dir: &Path,
    source: &Path,
    len: u64,
    modified: SystemTime,
) -> PathBuf {
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
    files.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let inflight = in_progress_bytes(cache_dir, keep);
    let mut used: u64 = files
        .iter()
        .map(|entry| entry.bytes)
        .sum::<u64>()
        .saturating_add(inflight);
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

/// Bytes of `.part` files that belong to a queued or running encode.
/// `skip_dest` is the finished name of the encode that is about to be stored,
/// so its part is not added twice on top of `incoming`.
fn in_progress_bytes(cache_dir: &Path, skip_dest: Option<&Path>) -> u64 {
    let hub = hub();
    let mut total = 0u64;
    for job in hub.jobs.values() {
        if !matches!(job.state, JobState::Running | JobState::Queued) {
            continue;
        }
        if job.cache_dir != cache_dir {
            continue;
        }
        if skip_dest.is_some_and(|dest| dest == job.dest) {
            continue;
        }
        let Ok(meta) = fs::metadata(&job.part) else {
            continue;
        };
        total = total.saturating_add(meta.len());
    }
    total
}

fn cache_accounted_bytes(cache_dir: &Path) -> u64 {
    cache_mp4s(cache_dir)
        .iter()
        .map(|entry| entry.bytes)
        .sum::<u64>()
        .saturating_add(in_progress_bytes(cache_dir, None))
}

/// Delete `.part` files left by a previous process. A part that belongs to an
/// encode still queued or running in this process is kept.
pub(crate) fn sweep_stale_parts(cache_dir: &Path) {
    let protected: Vec<PathBuf> = {
        let hub = hub();
        hub.jobs
            .values()
            .filter(|job| matches!(job.state, JobState::Running | JobState::Queued))
            .map(|job| job.part.clone())
            .collect()
    };
    let Ok(read) = fs::read_dir(cache_dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !name.ends_with(".part.mp4") || protected.iter().any(|live| live == &path) {
            continue;
        }
        let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        if fs::remove_file(&path).is_ok() {
            cast_log("tv_copy", &format!("sweep bytes={bytes}"));
        }
    }
}

/// Sweep the active cache. Called when the app starts, before any encode.
pub(crate) fn startup() {
    let dir = active_cache_dir();
    let _ = fs::create_dir_all(&dir);
    sweep_stale_parts(&dir);
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
    Hold { bytes: Vec<u8>, gate: TestGate },
    Ffmpeg(PathBuf),
}

#[derive(Clone)]
pub(crate) enum TestEncoder {
    Missing,
    Fail(String),
    Write { bytes: Vec<u8>, delay: Duration },
    Hold { bytes: Vec<u8>, gate: TestGate },
}

#[derive(Clone)]
pub(crate) struct TestGate {
    inner: std::sync::Arc<GateInner>,
}

struct GateInner {
    lock: Mutex<bool>,
    cv: Condvar,
    entered: AtomicUsize,
}

impl TestGate {
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(GateInner {
                lock: Mutex::new(false),
                cv: Condvar::new(),
                entered: AtomicUsize::new(0),
            }),
        }
    }

    fn wait_turn(&self) {
        self.inner.entered.fetch_add(1, Ordering::SeqCst);
        let mut released = self
            .inner
            .lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        while !*released {
            released = self
                .inner
                .cv
                .wait(released)
                .unwrap_or_else(|err| err.into_inner());
        }
    }

    pub(crate) fn entered(&self) -> usize {
        self.inner.entered.load(Ordering::SeqCst)
    }

    pub(crate) fn release(&self) {
        let mut released = self
            .inner
            .lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *released = true;
        self.inner.cv.notify_all();
    }
}

fn resolve_encoder() -> ActiveEncoder {
    if let Some(test) = TEST_ENCODER.with(|slot| slot.borrow().clone()) {
        return match test {
            TestEncoder::Missing => ActiveEncoder::Missing,
            TestEncoder::Fail(err) => ActiveEncoder::Fail(err),
            TestEncoder::Write { bytes, delay } => ActiveEncoder::Write { bytes, delay },
            TestEncoder::Hold { bytes, gate } => ActiveEncoder::Hold { bytes, gate },
        };
    }
    match bundled_ffmpeg() {
        Some(path) => ActiveEncoder::Ffmpeg(path),
        None => ActiveEncoder::Missing,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobState {
    Queued,
    Running,
    Ready,
    Failed,
}

struct JobSlot {
    state: JobState,
    cache_dir: PathBuf,
    part: PathBuf,
    dest: PathBuf,
}

#[derive(Clone)]
struct Failure {
    attempts: u32,
    retry_at: Instant,
}

struct Hub {
    jobs: HashMap<String, JobSlot>,
    ready: HashMap<PathBuf, PathBuf>,
    failed: HashMap<String, Failure>,
    queue: VecDeque<JobSpec>,
    /// Cast-check result for one path+size+mtime. Range requests reuse it.
    checks: HashMap<String, bool>,
}

fn hub() -> std::sync::MutexGuard<'static, Hub> {
    HUB.get_or_init(|| {
        Mutex::new(Hub {
            jobs: HashMap::new(),
            ready: HashMap::new(),
            failed: HashMap::new(),
            queue: VecDeque::new(),
            checks: HashMap::new(),
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
    static TEST_LIMIT_FILE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static TEST_ENCODER_FILE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static TEST_ENCODER_SETTING: std::cell::RefCell<Option<TvEncoder>> = const { std::cell::RefCell::new(None) };
    static TEST_BACKOFF: std::cell::RefCell<Option<Duration>> = const { std::cell::RefCell::new(None) };
    static COUNT_CAST_CHECKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CAST_CHECK_RUNS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// TV-copy tests share one process-wide encode cap. Holding a scope keeps
/// other scopes from starting until this one drops, so a queue assertion is
/// not racing another test's jobs.
static SCOPE_N: Mutex<usize> = Mutex::new(0);
static SCOPE_CV: Condvar = Condvar::new();

struct ScopePermit;

impl ScopePermit {
    fn acquire() -> Self {
        let mut n = SCOPE_N.lock().unwrap_or_else(|err| err.into_inner());
        while *n > 0 {
            n = SCOPE_CV.wait(n).unwrap_or_else(|err| err.into_inner());
        }
        *n += 1;
        Self
    }
}

impl Drop for ScopePermit {
    fn drop(&mut self) {
        let mut n = SCOPE_N.lock().unwrap_or_else(|err| err.into_inner());
        *n = n.saturating_sub(1);
        SCOPE_CV.notify_all();
    }
}

pub(crate) struct TestScope {
    cache: PathBuf,
    _permit: ScopePermit,
}

impl TestScope {
    pub(crate) fn new(cache: PathBuf, encoder: TestEncoder) -> Self {
        let permit = ScopePermit::acquire();
        let _ = fs::create_dir_all(&cache);
        TEST_CACHE.with(|slot| *slot.borrow_mut() = Some(cache.clone()));
        TEST_ENCODER.with(|slot| *slot.borrow_mut() = Some(encoder));
        Self {
            cache,
            _permit: permit,
        }
    }
}

impl Drop for TestScope {
    fn drop(&mut self) {
        TEST_CACHE.with(|slot| *slot.borrow_mut() = None);
        TEST_ENCODER.with(|slot| *slot.borrow_mut() = None);
        TEST_BACKOFF.with(|slot| *slot.borrow_mut() = None);
        let mut hub = hub();
        hub.queue.retain(|spec| spec.cache_dir != self.cache);
        hub.jobs.retain(|_, job| job.cache_dir != self.cache);
        hub.ready
            .retain(|_, dest| dest.parent() != Some(self.cache.as_path()));
    }
}

pub(crate) fn encode_cap() -> usize {
    MAX_CONCURRENT_ENCODES
}

/// Running keys, then queued keys in FIFO order, for one cache directory.
pub(crate) fn encode_slots(cache: &Path) -> (Vec<String>, Vec<String>) {
    let hub = hub();
    let running = hub
        .jobs
        .iter()
        .filter(|(_, job)| job.cache_dir == cache && job.state == JobState::Running)
        .map(|(key, _)| key.clone())
        .collect();
    let queued = hub
        .queue
        .iter()
        .filter(|spec| spec.cache_dir == cache)
        .map(|spec| spec.key.clone())
        .collect();
    (running, queued)
}

struct RunningMark {
    key: String,
}

impl Drop for RunningMark {
    fn drop(&mut self) {
        hub().jobs.remove(&self.key);
    }
}

fn test_mark_running(cache: &Path, part: &Path) -> RunningMark {
    let key = format!("live-{}", uuid_for_mark());
    hub().jobs.insert(
        key.clone(),
        JobSlot {
            state: JobState::Running,
            cache_dir: cache.to_path_buf(),
            part: part.to_path_buf(),
            dest: cache.join(format!("{key}.mp4")),
        },
    );
    RunningMark { key }
}

fn uuid_for_mark() -> String {
    format!(
        "{:x}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub(crate) fn set_test_encoder_file(path: Option<PathBuf>) {
    TEST_ENCODER_FILE.with(|slot| *slot.borrow_mut() = path);
}

/// Spawn ffmpeg and keep the child handle. `Command::output` would drop that
/// handle when the wait finished, and a killed parent would leave ffmpeg
/// running. The handle is duplicated into a process-wide list and, on Windows,
/// the process is assigned to a job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
fn spawn_tracked_ffmpeg(ffmpeg: &Path, args: &[String]) -> Result<Child, String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd
        .spawn()
        .map_err(|_| "could not start ffmpeg".to_string())?;
    track_child(&child);
    Ok(child)
}

fn wait_tracked(child: Child) -> Result<std::process::Output, String> {
    let pid = child.id();
    let out = child
        .wait_with_output()
        .map_err(|_| "could not start ffmpeg".to_string());
    untrack(pid);
    out
}

/// Kill every ffmpeg this process still has open, then close the Windows job
/// so `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` ends anything the handle missed.
pub(crate) fn shutdown_encodes() {
    let procs;
    #[cfg(windows)]
    let job;
    {
        let mut book = track_book();
        procs = std::mem::take(&mut book.procs);
        #[cfg(windows)]
        {
            job = book.job.take();
        }
    }
    for proc in &procs {
        terminate_proc(proc);
    }
    #[cfg(windows)]
    if let Some(job) = job {
        unsafe {
            TerminateJobObject(job.0, 1);
            CloseHandle(job.0);
        }
    }
    for proc in procs {
        close_proc(proc);
    }
}

struct TrackBook {
    procs: Vec<TrackedProc>,
    #[cfg(windows)]
    job: Option<SharedHandle>,
    #[cfg(windows)]
    last_assign_ok: bool,
    #[cfg(windows)]
    last_assign_error: u32,
}

struct TrackedProc {
    pid: u32,
    #[cfg(windows)]
    handle: *mut std::ffi::c_void,
}

#[cfg(windows)]
struct SharedHandle(*mut std::ffi::c_void);

unsafe impl Send for TrackedProc {}
#[cfg(windows)]
unsafe impl Send for SharedHandle {}
unsafe impl Send for TrackBook {}

fn track_book() -> std::sync::MutexGuard<'static, TrackBook> {
    TRACK
        .get_or_init(|| {
            Mutex::new(TrackBook {
                procs: Vec::new(),
                #[cfg(windows)]
                job: None,
                #[cfg(windows)]
                last_assign_ok: false,
                #[cfg(windows)]
                last_assign_error: 0,
            })
        })
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

static TRACK: std::sync::OnceLock<Mutex<TrackBook>> = std::sync::OnceLock::new();

fn track_child(child: &Child) {
    let mut book = track_book();
    #[cfg(windows)]
    {
        ensure_job(&mut book);
        let job_handle = book.job.as_ref().map(|job| job.0);
        if let Some(job_handle) = job_handle {
            use std::os::windows::io::AsRawHandle;
            let ok = unsafe {
                AssignProcessToJobObject(job_handle, child.as_raw_handle() as *mut std::ffi::c_void)
            };
            book.last_assign_ok = ok != 0;
            if ok == 0 {
                book.last_assign_error = unsafe { GetLastError() };
            }
        }
        book.procs.push(TrackedProc {
            pid: child.id(),
            handle: dup_process(child),
        });
    }
    #[cfg(not(windows))]
    {
        book.procs.push(TrackedProc { pid: child.id() });
    }
}

fn untrack(pid: u32) {
    let proc = {
        let mut book = track_book();
        let Some(index) = book.procs.iter().position(|proc| proc.pid == pid) else {
            return;
        };
        book.procs.remove(index)
    };
    close_proc(proc);
}

fn terminate_proc(proc: &TrackedProc) {
    #[cfg(windows)]
    unsafe {
        if !proc.handle.is_null() {
            TerminateProcess(proc.handle, 1);
        } else {
            let opened = OpenProcess(0x0001, 0, proc.pid);
            if !opened.is_null() {
                TerminateProcess(opened, 1);
                CloseHandle(opened);
            }
        }
    }
    #[cfg(not(windows))]
    unsafe {
        kill(proc.pid as i32, 9);
    }
}

fn close_proc(proc: TrackedProc) {
    #[cfg(windows)]
    unsafe {
        if !proc.handle.is_null() {
            CloseHandle(proc.handle);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = proc;
    }
}

pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        let opened = OpenProcess(0x1000, 0, pid);
        if opened.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(opened, &mut code);
        CloseHandle(opened);
        ok != 0 && code == 259
    }
    #[cfg(not(windows))]
    unsafe {
        kill(pid as i32, 0) == 0
    }
}

#[cfg(windows)]
fn ensure_job(book: &mut TrackBook) {
    if book.job.is_some() {
        return;
    }
    match create_kill_on_close_job() {
        Some(job) => book.job = Some(job),
        None => book.last_assign_error = unsafe { GetLastError() },
    }
}

#[cfg(windows)]
fn dup_process(child: &Child) -> *mut std::ffi::c_void {
    use std::os::windows::io::AsRawHandle;
    unsafe {
        let mut target = std::ptr::null_mut();
        let ok = DuplicateHandle(
            GetCurrentProcess(),
            child.as_raw_handle() as *mut std::ffi::c_void,
            GetCurrentProcess(),
            &mut target,
            0,
            0,
            0x0000_0002,
        );
        if ok == 0 {
            std::ptr::null_mut()
        } else {
            target
        }
    }
}

#[cfg(windows)]
pub(crate) fn ffmpeg_in_kill_job(pid: u32) -> bool {
    let book = track_book();
    let Some(job) = book.job.as_ref() else {
        return false;
    };
    unsafe {
        let opened = OpenProcess(0x0400, 0, pid);
        if opened.is_null() {
            return false;
        }
        let mut inside = 0i32;
        let ok = IsProcessInJob(opened, job.0, &mut inside);
        CloseHandle(opened);
        ok != 0 && inside != 0
    }
}

#[cfg(windows)]
pub(crate) fn last_assign_error() -> u32 {
    let book = track_book();
    if book.last_assign_ok {
        0
    } else {
        book.last_assign_error
    }
}

#[cfg(windows)]
fn create_kill_on_close_job() -> Option<SharedHandle> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info = std::mem::zeroed::<JobExtendedLimit>();
        info.basic.limit_flags = 0x0000_2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        let ok = SetInformationJobObject(
            job,
            9, // JobObjectExtendedLimitInformation
            &info as *const JobExtendedLimit as *const std::ffi::c_void,
            std::mem::size_of::<JobExtendedLimit>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return None;
        }
        Some(SharedHandle(job))
    }
}

#[cfg(windows)]
#[repr(C)]
struct JobBasicLimit {
    per_process_user_time: i64,
    per_job_user_time: i64,
    limit_flags: u32,
    minimum_working_set: usize,
    maximum_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[cfg(windows)]
#[repr(C)]
struct JobIoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[cfg(windows)]
#[repr(C)]
struct JobExtendedLimit {
    basic: JobBasicLimit,
    io: JobIoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[cfg(windows)]
extern "system" {
    fn CreateJobObjectW(attrs: *mut std::ffi::c_void, name: *const u16) -> *mut std::ffi::c_void;
    fn SetInformationJobObject(
        job: *mut std::ffi::c_void,
        class: i32,
        info: *const std::ffi::c_void,
        len: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: *mut std::ffi::c_void, process: *mut std::ffi::c_void) -> i32;
    fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn TerminateProcess(handle: *mut std::ffi::c_void, exit_code: u32) -> i32;
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn DuplicateHandle(
        source_process: *mut std::ffi::c_void,
        source: *mut std::ffi::c_void,
        target_process: *mut std::ffi::c_void,
        target: *mut *mut std::ffi::c_void,
        access: u32,
        inherit: i32,
        options: u32,
    ) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn GetExitCodeProcess(handle: *mut std::ffi::c_void, code: *mut u32) -> i32;
    fn IsProcessInJob(
        process: *mut std::ffi::c_void,
        job: *mut std::ffi::c_void,
        result: *mut i32,
    ) -> i32;
    fn GetLastError() -> u32;
}

#[cfg(not(windows))]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
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
        args.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
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
        assert!(written.contains("tv_copy evict bytes=300"), "{written}");
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
        let matrix_only = "Stream #0:0: Video: h264 (High 10), yuv420p10le(tv, bt2020nc/unknown/unknown, progressive), 160x120, 24 fps";
        let (_, _, _, hdr, transfer) = parse_video_line(matrix_only).unwrap();
        assert!(hdr, "10-bit is still HDR-or-10-bit");
        assert_eq!(transfer, "bt709");
    }

    #[test]
    fn a_third_encode_waits_in_the_fifo_queue() {
        assert_eq!(encode_cap(), 2, "the cap is two simultaneous encodes");
        let root = std::env::temp_dir().join(format!("slidex-tv-queue-{}", uuid_tail()));
        let cache = root.join("cache");
        let logs = root.join("logs");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&logs).unwrap();
        let _log = super::super::push_cast_log_dir(logs);
        let gate = TestGate::new();
        let _scope = TestScope::new(
            cache.clone(),
            TestEncoder::Hold {
                bytes: b"not a video".to_vec(),
                gate: gate.clone(),
            },
        );
        let mut paths = Vec::new();
        let mut keys = Vec::new();
        for name in ["one.mkv", "two.mkv", "three.mkv"] {
            let path = root.join(name);
            fs::write(&path, b"\x1a\x45\xdf\xa3queue").unwrap();
            let path = path.canonicalize().unwrap_or(path);
            let meta = fs::metadata(&path).unwrap();
            keys.push(cache_key(
                &path,
                meta.len(),
                meta.modified().unwrap_or(UNIX_EPOCH),
            ));
            paths.push(path);
        }
        for path in &paths {
            match for_cast(path, 1) {
                ForCast::Preparing { .. } => {}
                ForCast::Original { .. } | ForCast::Copy { .. } | ForCast::Skip { .. } => {
                    panic!("encode should be pending for {}", path.display());
                }
            }
        }
        let started = Instant::now();
        while gate.entered() < 2 {
            if started.elapsed() > Duration::from_secs(5) {
                panic!("the two running encodes did not reach the encoder");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let (running, queued) = encode_slots(&cache);
        assert_eq!(
            running.len(),
            2,
            "two encodes run and the third waits: {running:?}"
        );
        assert_eq!(
            queued,
            vec![keys[2].clone()],
            "the third job waits at the head of the FIFO queue"
        );
        assert!(running.contains(&keys[0]), "{running:?}");
        assert!(running.contains(&keys[1]), "{running:?}");
        assert_eq!(gate.entered(), 2, "the queued job must not have started");
        gate.release();
        assert!(
            wait_until_settled(Duration::from_secs(5)),
            "queued encodes should finish after a slot frees"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_parts_are_swept_at_startup() {
        let root = std::env::temp_dir().join(format!("slidex-tv-part-{}", uuid_tail()));
        let cache = root.join("tv-cache");
        let logs = root.join("logs");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&logs).unwrap();
        let _guard = super::super::push_cast_log_dir(logs.clone());
        let _scope = TestScope::new(cache.clone(), TestEncoder::Missing);
        let stale = cache.join("orphan.part.mp4");
        fs::write(&stale, vec![1u8; 1000]).unwrap();
        let finished = cache.join("done.mp4");
        fs::write(&finished, vec![2u8; 400]).unwrap();
        let live = cache.join("live.part.mp4");
        fs::write(&live, vec![3u8; 500]).unwrap();
        let _mark = test_mark_running(&cache, &live);
        startup();
        assert!(
            !stale.exists(),
            "a part from a previous run is swept at startup"
        );
        assert!(live.exists(), "a part that is still in progress is kept");
        assert!(finished.exists());
        assert_eq!(cache_accounted_bytes(&cache), 900);
        evict_to_fit(&cache, 600, 0, None);
        assert!(
            !finished.exists(),
            "in-progress part bytes count against the cache limit"
        );
        assert!(
            live.exists(),
            "eviction does not delete the in-progress part"
        );
        let written = fs::read_to_string(logs.join("cast.log")).unwrap_or_default();
        assert!(written.contains("tv_copy sweep bytes=1000"), "{written}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_key_is_retried_after_backoff() {
        let root = std::env::temp_dir().join(format!("slidex-tv-retry-{}", uuid_tail()));
        let cache = root.join("cache");
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let _guard = super::super::push_cast_log_dir(logs.clone());
        let _scope = TestScope::new(
            cache,
            TestEncoder::Fail("encoder rejected the settings".into()),
        );
        TEST_BACKOFF.with(|slot| *slot.borrow_mut() = Some(Duration::from_millis(80)));
        let src = root.join("clip.mkv");
        fs::write(&src, b"\x1a\x45\xdf\xa3retry").unwrap();
        assert!(matches!(for_cast(&src, 2), ForCast::Preparing { .. }));
        assert!(wait_until_settled(Duration::from_secs(5)));
        assert!(
            matches!(for_cast(&src, 2), ForCast::Skip { .. }),
            "a failed key stays failed until the backoff elapses"
        );
        thread::sleep(Duration::from_millis(120));
        assert!(
            matches!(for_cast(&src, 2), ForCast::Preparing { .. }),
            "the same key is encoded again after the backoff"
        );
        assert!(wait_until_settled(Duration::from_secs(5)));
        let written = fs::read_to_string(logs.join("cast.log")).unwrap_or_default();
        assert_eq!(written.matches("tv_copy start").count(), 2, "{written}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cast_check_is_cached_per_key() {
        let root = std::env::temp_dir().join(format!("slidex-tv-check-{}", uuid_tail()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("clip.mp4");
        fs::write(&path, b"not-a-real-mp4-box").unwrap();
        COUNT_CAST_CHECKS.with(|flag| flag.set(true));
        CAST_CHECK_RUNS.with(|n| n.set(0));
        assert!(!copy_passes(&path));
        assert!(!copy_passes(&path));
        assert_eq!(
            CAST_CHECK_RUNS.with(|n| n.get()),
            1,
            "a second range request for the same key must not re-read the file"
        );
        COUNT_CAST_CHECKS.with(|flag| flag.set(false));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_limit_reads_a_setting_or_a_byte_suffix() {
        assert_eq!(parse_cache_limit("20GB"), Some(20 * 1024 * 1024 * 1024));
        assert_eq!(parse_cache_limit("2 MB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_cache_limit("4096"), Some(4096));
        assert!(parse_cache_limit("1.5GB").is_none());
        let root = std::env::temp_dir().join(format!("slidex-tv-limit-{}", uuid_tail()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("tv-cache-limit");
        fs::write(&file, "3MB\n").unwrap();
        if std::env::var("SLIDEX_TV_CACHE_LIMIT").is_err() {
            TEST_LIMIT_FILE.with(|slot| *slot.borrow_mut() = Some(file));
            assert_eq!(active_limit(), 3 * 1024 * 1024);
            TEST_LIMIT_FILE.with(|slot| *slot.borrow_mut() = None);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gpl_notice_names_x264_and_the_ffmpeg_source() {
        let notice = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("ffmpeg-gpl-notice.txt");
        let text = fs::read_to_string(&notice).unwrap();
        let lower = text.to_ascii_lowercase();
        assert!(
            lower.contains("gnu general public license") || lower.contains("gpl"),
            "{text}"
        );
        assert!(lower.contains("x264"), "{text}");
        assert!(
            text.contains("https://github.com/FFmpeg/FFmpeg/commit/946fcce07b"),
            "{text}"
        );
        assert!(
            text.contains(
                "https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-full_build.7z"
            ),
            "{text}"
        );
    }

    #[test]
    fn synthetic_clip_passes_the_cast_check_when_ffmpeg_is_present() {
        with_pinned_ffmpeg(|ffmpeg| {
            let root = std::env::temp_dir().join(format!("slidex-tv-synth-{}", uuid_tail()));
            fs::create_dir_all(&root).unwrap();
            let src = root.join("src.mp4");
            let out = root.join("tv.mp4");
            assert!(
                lavfi(
                    ffmpeg,
                    &[
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
                    ],
                    &src
                ),
                "could not write the synthetic source"
            );
            assert!(
                super::super::decide_cast_video(&src).skip.is_some(),
                "the 90 fps 10-bit source is not cast as-is"
            );
            encode_with_ffmpeg(ffmpeg, &src, &out, TvEncoder::Libx264).unwrap();
            let decision = super::super::decide_cast_video(&out);
            assert!(
                decision.skip.is_none(),
                "the TV copy must pass the cast check: {:?}",
                decision.skip
            );
            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn unicode_filename_with_spaces_passes_the_cast_check() {
        with_pinned_ffmpeg(|ffmpeg| {
            let root = std::env::temp_dir().join(format!("slidex-tv-uni-{}", uuid_tail()));
            fs::create_dir_all(&root).unwrap();
            let src = root.join("café night 画面.mkv");
            let out = root.join("TV コピー.mp4");
            assert!(
                lavfi(
                    ffmpeg,
                    &[
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc=size=64x64:rate=24:duration=0.3",
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:sample_rate=48000:duration=0.3",
                        "-c:v",
                        "libx264",
                        "-pix_fmt",
                        "yuv420p",
                        "-profile:v",
                        "high",
                        "-c:a",
                        "aac",
                        "-shortest",
                    ],
                    &src
                ),
                "could not write {}",
                src.display()
            );
            encode_with_ffmpeg(ffmpeg, &src, &out, TvEncoder::Libx264).unwrap();
            let decision = super::super::decide_cast_video(&out);
            assert!(
                decision.skip.is_none(),
                "unicode path with spaces must pass the cast check: {:?}",
                decision.skip
            );
            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn ninety_fps_source_passes_the_cast_check() {
        with_pinned_ffmpeg(|ffmpeg| {
            let root = std::env::temp_dir().join(format!("slidex-tv-fps-{}", uuid_tail()));
            fs::create_dir_all(&root).unwrap();
            let src = root.join("fast.mp4");
            let out = root.join("tv.mp4");
            assert!(lavfi(
                ffmpeg,
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=64x64:rate=90:duration=0.3",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000:duration=0.3",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-profile:v",
                    "high",
                    "-c:a",
                    "aac",
                    "-shortest",
                ],
                &src
            ));
            let probed = probe_source(ffmpeg, &src).unwrap();
            assert!(probed.fps > 60.0, "source fps {}", probed.fps);
            assert!(!probed.hdr_or_10bit);
            assert!(
                super::super::decide_cast_video(&src).skip.is_some(),
                "90 fps is not cast as-is"
            );
            encode_with_ffmpeg(ffmpeg, &src, &out, TvEncoder::Libx264).unwrap();
            let decision = super::super::decide_cast_video(&out);
            assert!(
                decision.skip.is_none(),
                "the 60 fps copy must pass the cast check: {:?}",
                decision.skip
            );
            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn ten_bit_hdr_source_passes_the_cast_check() {
        with_pinned_ffmpeg(|ffmpeg| {
            let root = std::env::temp_dir().join(format!("slidex-tv-hdr-{}", uuid_tail()));
            fs::create_dir_all(&root).unwrap();
            let src = root.join("hdr.mp4");
            let out = root.join("tv.mp4");
            assert!(
                lavfi(
                    ffmpeg,
                    &[
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc=size=160x120:rate=24:duration=0.3",
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:sample_rate=48000:duration=0.3",
                        "-c:v",
                        "libx264",
                        "-pix_fmt",
                        "yuv420p10le",
                        "-profile:v",
                        "high10",
                        "-x264-params",
                        "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc",
                        "-c:a",
                        "aac",
                        "-shortest",
                    ],
                    &src
                ),
                "could not write the 10-bit HDR source"
            );
            let probed = probe_source(ffmpeg, &src).unwrap();
            assert!(probed.hdr_or_10bit, "probe did not see 10-bit or HDR");
            assert_eq!(
                probed.transfer_in, "smpte2084",
                "the source must be PQ, not only a BT.2020 matrix"
            );
            assert!(
                super::super::decide_cast_video(&src).skip.is_some(),
                "10-bit HDR is not cast as-is"
            );
            encode_with_ffmpeg(ffmpeg, &src, &out, TvEncoder::Libx264).unwrap();
            let decision = super::super::decide_cast_video(&out);
            assert!(
                decision.skip.is_none(),
                "the tone-mapped copy must pass the cast check: {:?}",
                decision.skip
            );
            let copied = probe_source(ffmpeg, &out).unwrap();
            assert!(!copied.hdr_or_10bit, "the copy is still 10-bit or HDR");
            assert_eq!(copied.transfer_in, "bt709");
            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn ffmpeg_is_gone_after_shutdown() {
        with_pinned_ffmpeg(|ffmpeg| {
            let root = std::env::temp_dir().join(format!("slidex-tv-kill-{}", uuid_tail()));
            fs::create_dir_all(&root).unwrap();
            let sink = root.join("sink.mkv");
            let child = spawn_tracked_ffmpeg(
                ffmpeg,
                &[
                    "-hide_banner".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-re".into(),
                    "-f".into(),
                    "lavfi".into(),
                    "-i".into(),
                    "testsrc=size=32x32:rate=5:duration=30".into(),
                    "-an".into(),
                    "-c:v".into(),
                    "libx264".into(),
                    sink.display().to_string(),
                ],
            )
            .unwrap();
            let pid = child.id();
            let started = Instant::now();
            while !process_alive(pid) {
                if started.elapsed() > Duration::from_secs(8) {
                    let detail = ffmpeg_exit_detail(child);
                    panic!("ffmpeg did not stay running: {detail}");
                }
                thread::sleep(Duration::from_millis(20));
            }
            thread::sleep(Duration::from_millis(400));
            if !process_alive(pid) {
                let detail = ffmpeg_exit_detail(child);
                panic!("ffmpeg exited before shutdown: {detail}");
            }
            #[cfg(windows)]
            assert!(
                ffmpeg_in_kill_job(pid),
                "ffmpeg was not assigned to the kill-on-close job (win32 {})",
                last_assign_error()
            );
            shutdown_encodes();
            let _ = child.wait_with_output();
            let deadline = Instant::now() + Duration::from_secs(5);
            while process_alive(pid) {
                if Instant::now() > deadline {
                    panic!("ffmpeg {pid} is still running after shutdown");
                }
                thread::sleep(Duration::from_millis(30));
            }
            let _ = fs::remove_dir_all(&root);
        });
    }

    fn ffmpeg_exit_detail(child: Child) -> String {
        match child.wait_with_output() {
            Ok(out) => format!(
                "status {:?} stderr {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(err) => format!("wait failed: {err}"),
        }
    }

    fn lavfi(ffmpeg: &Path, args: &[&str], output: &Path) -> bool {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        cmd.args(args);
        cmd.arg(output);
        cmd.status().map(|status| status.success()).unwrap_or(false)
    }

    static FFMPEG_LOCK: Mutex<()> = Mutex::new(());

    fn ffmpeg_runs(path: &Path) -> bool {
        Command::new(path)
            .arg("-version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// The pinned Windows binary when it runs. Otherwise a system ffmpeg, so a
    /// Linux checkout can still exercise the encode. `SLIDEX_REQUIRE_FFMPEG=1`
    /// refuses that fallback.
    fn with_pinned_ffmpeg(body: impl FnOnce(&Path)) {
        let _lock = FFMPEG_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let require = std::env::var("SLIDEX_REQUIRE_FFMPEG").ok().as_deref() == Some("1");
        if let Some(ffmpeg) = pinned_ffmpeg_exe() {
            if ffmpeg_runs(&ffmpeg) {
                body(&ffmpeg);
                return;
            }
            if require {
                panic!("pinned ffmpeg.exe did not run");
            }
        } else if require {
            panic!("pinned ffmpeg.exe is missing");
        }
        let Some(system) = system_ffmpeg() else {
            eprintln!("pinned ffmpeg.exe is not runnable here; skipping the synthetic encode");
            return;
        };
        eprintln!(
            "pinned ffmpeg.exe is not runnable; using {}",
            system.display()
        );
        body(&system);
    }

    fn system_ffmpeg() -> Option<PathBuf> {
        if !ffmpeg_runs(Path::new("ffmpeg")) {
            return None;
        }
        let out = Command::new("which").arg("ffmpeg").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
        if path.is_file() {
            Some(path)
        } else {
            None
        }
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
