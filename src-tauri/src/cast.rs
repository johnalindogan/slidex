//! Chromecast / Google TV content cast.
//! Stack: mdns-sd discovery + rust_cast (Cast V2) + a LAN-only HTTP server
//! → Default Media Receiver. Not chrome.cast, not desktop mirror, no cloud relay.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent};
use rust_cast::channels::connection::ConnectionChannel;
use rust_cast::channels::heartbeat::{HeartbeatChannel, HeartbeatResponse};
use rust_cast::channels::media::{IdleReason, MediaChannel, MediaResponse, PlayerState};
use rust_cast::channels::receiver::{Application, ReceiverChannel, ReceiverResponse};
use rust_cast::message_manager::{CastMessage, CastMessagePayload, MessageManager};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    crypto::{aws_lc_rs::default_provider, verify_tls12_signature, verify_tls13_signature},
    ClientConfig, ClientConnection, DigitallySignedStruct, StreamOwned,
};
use serde::Serialize;
use uuid::Uuid;

const CAST_SERVICE: &str = "_googlecast._tcp.local.";
const DISCOVER_DEFAULT_MS: u64 = 8000;
const DISCOVER_MAX_MS: u64 = 10_000;
/// Inbound TCP range the installer opens on Private networks. Keep in sync with
/// `CAST_FW_TCP` in `src-tauri/windows/installer.nsi`.
pub const CAST_PORT_LO: u16 = 47200;
pub const CAST_PORT_HI: u16 = 47215;
const CAST_APP_ID_PORT: u16 = 8009;
const READ_IDLE: Duration = Duration::from_millis(350);
/// Slice used while waiting for LAUNCH or LOAD so a PING can be answered.
const READ_SLICE: Duration = Duration::from_millis(400);
/// Cold TVs often take 5–10s to answer LAUNCH. Give up at 15s.
const LAUNCH_DEADLINE: Duration = Duration::from_secs(15);
const LOAD_DEADLINE: Duration = Duration::from_secs(15);
/// Photo hold when the page does not send a duration. A positive session value replaces this.
const DEFAULT_PHOTO_TIMER_MS: u64 = 10_000;
/// Our heartbeat. The TV is treated as gone after `LINK_DEAD_AFTER` of silence.
const PING_EVERY: Duration = Duration::from_secs(5);
const LINK_DEAD_AFTER: Duration = Duration::from_secs(15);
/// Launch can use the full 15s, plus TCP and TLS.
const SESSION_READY_WAIT: Duration = Duration::from_secs(20);
const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
const CAST_LOG_MAX: u64 = 1_000_000;
const CAST_CANCELLED: &str = "Cast stopped before this request was sent.";

/// Who asked for a Cast command. Media element events are not a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastOrigin {
    User,
    Timer,
    Remote,
    /// The session-ready wait gave up. The page cannot send this source.
    Timeout,
}

impl CastOrigin {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "user" => Ok(Self::User),
            "timer" => Ok(Self::Timer),
            "remote" => Ok(Self::Remote),
            _ => Err("Cast command source must be user, timer, or remote.".into()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Timer => "timer",
            Self::Remote => "remote",
            Self::Timeout => "timeout",
        }
    }
}

/// Milliseconds a photo stays up. The page's duration is used as sent.
/// Missing or zero is the 10s default, which is what a cast log showed
/// when the session duration never reached the timer.
fn photo_timer_ms(session_ms: Option<u64>) -> u64 {
    match session_ms {
        Some(ms) if ms > 0 => ms,
        _ => DEFAULT_PHOTO_TIMER_MS,
    }
}

fn with_timer_dur(detail: String, origin: CastOrigin, session_ms: Option<u64>) -> String {
    if origin != CastOrigin::Timer {
        return detail;
    }
    let ms = photo_timer_ms(session_ms);
    detail.replacen("source=timer", &format!("source=timer dur_ms={ms}"), 1)
}

fn connect_fail(detail: String) -> String {
    cast_log("error", &detail);
    detail
}

/// Connect failures use the same key=value shape as the other cast.log lines.
/// `host` is the address the connect was given. The reason stays the sentence
/// the panel shows. Neither value is a path.
fn log_connect_error(reason: &str, host: Ipv4Addr) {
    cast_log("error", &format!("reason=\"{reason}\" host={host}"));
}

/// One Disconnect, one line, including when the session and the caller both try.
fn log_disconnect_once(flag: &AtomicBool, detail: &str) {
    if flag
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        cast_log("disconnect", detail);
    }
}

pub const LAUNCH_TIMEOUT_MESSAGE: &str =
    "The TV didn't answer the launch request. Make sure it's on and try again.";
/// The control channel is up, but the TV did not fetch the slide or did not answer a media command.
pub const MEDIA_FETCH_MESSAGE: &str =
    "The TV couldn't load the media from this PC. Check that the PC and TV are on the same network and that SlideX's Cast firewall rules are allowed.";

/// PLAY, PAUSE, and SEEK. The UI's reply wait is longer, so a live TV is not
/// reported unreachable while this wait is still running.
const MEDIA_CMD_DEADLINE: Duration = Duration::from_secs(4);
const MEDIA_CMD_REPLY_WAIT: Duration = Duration::from_secs(5);

pub const NO_TV_ERROR: &str = "No TV found on this network. Check that the PC and TV are on the same Wi-Fi, the firewall, or the router's client isolation.";

const UNSUPPORTED_MEDIA: &str =
    "Unsupported media. Cast plays JPEG, PNG, and MP4 (H.264 + AAC). Skipped.";

/// Shown when Windows has the home LAN marked Public, so Private-only firewall rules do not apply.
pub const PUBLIC_WIFI_MESSAGE: &str =
    "This Wi-Fi is set to Public in Windows. Set it to Private to cast.";

pub const MEDIA_LOAD_TIMEOUT_MESSAGE: &str =
    "The TV was found, but the photo or video did not load within 15 seconds.";

const HTTP_CONN_CAP: usize = 8;
const HTTP_CHUNK: usize = 64 * 1024;
/// No socket SO_RCVTIMEO/SO_SNDTIMEO. A Windows timeout leaves the socket undefined,
/// and a retried write can drop or repeat bytes. Stall with non-blocking WouldBlock instead.
const HTTP_POLL: Duration = Duration::from_millis(8);
const HTTP_IDLE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetCategory {
    Public,
    /// Private or DomainAuthenticated.
    Private,
    /// The category could not be read. Do not guess.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastNotice {
    NoTv,
    MediaLoadTimeout,
}

/// Public always uses the Windows Public message. Private and Unknown keep the scenario text.
pub fn cast_notice(scenario: CastNotice, category: NetCategory) -> String {
    if category == NetCategory::Public {
        return PUBLIC_WIFI_MESSAGE.to_string();
    }
    match scenario {
        CastNotice::NoTv => NO_TV_ERROR.to_string(),
        CastNotice::MediaLoadTimeout => MEDIA_LOAD_TIMEOUT_MESSAGE.to_string(),
    }
}

fn media_load_notice() -> String {
    // A LOAD that times out still has a live control channel. That is a media
    // fetch failure, not "TV unreachable", and it does not wait on PowerShell.
    cast_panel_message(CastPanelCause::MediaCommandTimeout)
}

/// A cast that never starts on a Public network is the firewall, not a missing TV.
fn with_public_network(existing: String) -> String {
    if home_lan_category() == NetCategory::Public {
        PUBLIC_WIFI_MESSAGE.to_string()
    } else {
        existing
    }
}

/// How long a network-profile query may run before the child is killed.
const LAN_CATEGORY_WAIT: Duration = Duration::from_millis(2500);

#[cfg(windows)]
struct LanCache {
    value: NetCategory,
    done: bool,
}

#[cfg(windows)]
fn lan_cache() -> &'static (Mutex<LanCache>, std::sync::Condvar, AtomicBool) {
    use std::sync::{Condvar, OnceLock};
    static CACHE: OnceLock<(Mutex<LanCache>, Condvar, AtomicBool)> = OnceLock::new();
    CACHE.get_or_init(|| {
        (
            Mutex::new(LanCache {
                value: NetCategory::Unknown,
                done: false,
            }),
            Condvar::new(),
            AtomicBool::new(false),
        )
    })
}

/// Start the profile query if it is not already running. Returns immediately.
#[cfg(windows)]
fn start_lan_category_query() {
    {
        let (lock, cv, started) = lan_cache();
        if started.swap(true, Ordering::SeqCst) {
            return;
        }
        let spawned = thread::Builder::new()
            .name("cast-lan-category".into())
            .spawn(move || {
                let alias = snapshot_ifaces().bind.map(|b| b.name);
                let value = windows_lan_category(alias.as_deref());
                if let Ok(mut slot) = lock.lock() {
                    slot.value = value;
                    slot.done = true;
                }
                cv.notify_all();
            });
        if spawned.is_err() {
            if let Ok(mut slot) = lan_cache().0.lock() {
                slot.done = true;
            }
            lan_cache().1.notify_all();
        }
    }
}

/// Cached category. Never waits. This is what the Cast session thread uses.
fn lan_category_now() -> NetCategory {
    #[cfg(windows)]
    {
        start_lan_category_query();
        return lan_cache()
            .0
            .lock()
            .map(|slot| slot.value)
            .unwrap_or(NetCategory::Unknown);
    }
    #[cfg(not(windows))]
    {
        NetCategory::Unknown
    }
}

fn home_lan_category() -> NetCategory {
    #[cfg(windows)]
    {
        start_lan_category_query();
        let (lock, cv, _) = lan_cache();
        let Ok(guard) = lock.lock() else {
            return NetCategory::Unknown;
        };
        let (guard, _) = cv
            .wait_timeout_while(guard, LAN_CATEGORY_WAIT, |slot| !slot.done)
            .unwrap_or_else(|err| err.into_inner());
        return guard.value;
    }
    #[cfg(not(windows))]
    {
        NetCategory::Unknown
    }
}

/// Interface alias only (Wi-Fi, Ethernet). The SSID is a different property and is not read.
#[cfg(windows)]
const LAN_PROFILE_SCRIPT: &str =
    "Get-NetConnectionProfile | ForEach-Object { $_.InterfaceAlias + \"`t\" + [string]$_.NetworkCategory }";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(windows)]
fn powershell_profile_command() -> std::process::Command {
    let program = powershell_exe_under_windows_dir(&windows_dir_from_env());
    let mut cmd = std::process::Command::new(program);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        LAN_PROFILE_SCRIPT,
    ]);
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(windows)]
fn windows_lan_category(alias: Option<&str>) -> NetCategory {
    net_category_from_profile_command(alias, powershell_profile_command(), LAN_CATEGORY_WAIT)
}

/// Run `cmd` and map its profile listing to a category. A hang returns
/// `Unknown` when `limit` elapses; the child is killed.
fn net_category_from_profile_command(
    alias: Option<&str>,
    cmd: std::process::Command,
    limit: Duration,
) -> NetCategory {
    let Some(alias) = alias else {
        return NetCategory::Unknown;
    };
    let Some(output) = output_bounded(cmd, limit) else {
        return NetCategory::Unknown;
    };
    if !output.status.success() {
        return NetCategory::Unknown;
    }
    category_from_profile_text(alias, &String::from_utf8_lossy(&output.stdout))
}

fn category_from_profile_text(alias: &str, text: &str) -> NetCategory {
    for line in text.lines() {
        let Some((name, category)) = line.trim().rsplit_once('\t') else {
            continue;
        };
        if name.eq_ignore_ascii_case(alias) {
            return parse_net_category(category);
        }
    }
    NetCategory::Unknown
}

/// `cmd` runs on another thread. `recv_timeout` is the deadline; on timeout the child is killed.
fn output_bounded(mut cmd: std::process::Command, limit: Duration) -> Option<std::process::Output> {
    use std::process::{Output, Stdio};
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out_thread = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stdout {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        let err_thread = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        let _ = tx.send((stdout, stderr));
    });
    match rx.recv_timeout(limit) {
        Ok((stdout, stderr)) => {
            let status = match child.try_wait() {
                Ok(Some(status)) => status,
                _ => {
                    let _ = child.kill();
                    child.wait().ok()?
                }
            };
            Some(Output {
                status,
                stdout,
                stderr,
            })
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

fn hanging_process_command() -> std::process::Command {
    #[cfg(windows)]
    {
        let mut cmd =
            std::process::Command::new(powershell_exe_under_windows_dir(&windows_dir_from_env()));
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        cmd
    }
}

fn parse_net_category(raw: &str) -> NetCategory {
    let s = raw.trim().to_ascii_lowercase();
    if s == "public" {
        NetCategory::Public
    } else if s == "private" || s.starts_with("domain") {
        NetCategory::Private
    } else {
        NetCategory::Unknown
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CastDeviceInfo {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CastDiscoverResult {
    pub devices: Vec<CastDeviceInfo>,
    pub timeout_ms: u64,
    pub interfaces: Vec<String>,
    pub error: Option<String>,
    pub diagnostic: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CastConnectInfo {
    pub device_name: String,
    pub device_host: String,
    pub device_port: u16,
    pub bind_ip: String,
    pub http_port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaylistUpdate {
    pub registered: usize,
    pub skipped: Vec<String>,
    /// 1-based index in the path list the page sent, for each file that was registered.
    /// A skipped file keeps its slot, so later items stay aligned with the page's `cur`.
    pub page_index: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CastLiveStatus {
    pub connected: bool,
    pub player_state: String,
    pub remote_event: Option<String>,
    pub error: Option<String>,
    pub media_kind: String,
    pub http_port: u16,
    pub bind_ip: String,
    /// 1-based playlist index now on the TV. 0 until the first LOAD.
    pub item_index: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CastFirewallStatus {
    pub state: String,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct IfaceCand {
    pub name: String,
    pub ip: Ipv4Addr,
    pub prefix: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfaceRole {
    Preferred,
    Skip,
}

/// Shown when the only routes are VPN, Tailscale, or a virtual adapter the TV cannot reach.
pub const NO_HOME_LAN_MESSAGE: &str =
    "This PC has no home-network address to serve photos from. Disconnect VPN or Tailscale and try again.";

/// Home-LAN addresses we will bind and browse. Public, Tailscale, VPN, virtual, and link-local are skipped.
pub fn iface_role(name: &str, ip: Ipv4Addr) -> IfaceRole {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
        return IfaceRole::Skip;
    }
    if is_link_local_v4(ip)
        || is_cgnat_v4(ip)
        || is_tailscale_named(name)
        || is_virtual_or_vpn_named(name)
    {
        return IfaceRole::Skip;
    }
    if is_rfc1918_v4(ip) {
        IfaceRole::Preferred
    } else {
        IfaceRole::Skip
    }
}

pub fn is_rfc1918_v4(ip: Ipv4Addr) -> bool {
    ip.is_private()
}

pub fn is_link_local_v4(ip: Ipv4Addr) -> bool {
    ip.is_link_local()
}

/// 100.64.0.0/10 — Tailscale's usual range.
pub fn is_cgnat_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

pub fn is_tailscale_named(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("tailscale") || n.contains("zerotier")
}

/// NordLynx, Hyper-V, and other virtual NICs often hold an RFC1918 address.
/// The TV is not on those subnets, so they are not bind candidates.
pub fn is_virtual_or_vpn_named(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "vethernet",
        "hyper-v",
        "default switch",
        "nordlynx",
        "wireguard",
        "tap-",
        "wintun",
        "openvpn",
        "vpn",
        "virtualbox",
        "vmware",
        "wsl",
    ];
    let n = name.to_ascii_lowercase();
    MARKERS.iter().any(|marker| n.contains(marker))
}

pub fn is_safe_bind_ip(ip: Ipv4Addr) -> bool {
    iface_role("", ip) == IfaceRole::Preferred
}

fn prefix_rank(ip: Ipv4Addr) -> u8 {
    let o = ip.octets();
    if o[0] == 192 && o[1] == 168 {
        0
    } else if o[0] == 10 {
        1
    } else {
        2
    }
}

/// Pick the home-LAN address to bind. Prefers the default-route address when it is RFC1918,
/// otherwise a 192.168 address over 10/8 and 172.16/12. Never returns public, Tailscale, or 0.0.0.0.
pub fn choose_bind_ip<'a>(
    ifaces: &'a [IfaceCand],
    default_route: Option<Ipv4Addr>,
) -> Option<&'a IfaceCand> {
    let mut preferred: Vec<&IfaceCand> = ifaces
        .iter()
        .filter(|i| iface_role(&i.name, i.ip) == IfaceRole::Preferred)
        .collect();
    if preferred.is_empty() {
        return None;
    }
    if let Some(ip) = default_route {
        if let Some(hit) = preferred.iter().copied().find(|i| i.ip == ip) {
            return Some(hit);
        }
    }
    preferred.sort_by(|a, b| {
        prefix_rank(a.ip)
            .cmp(&prefix_rank(b.ip))
            .then(a.name.cmp(&b.name))
            .then(a.ip.cmp(&b.ip))
    });
    preferred.first().copied()
}

/// After a TV is found, bind the physical adapter whose subnet contains that TV.
/// A default-route VPN address is not a candidate. None means [`NO_HOME_LAN_MESSAGE`].
pub fn choose_bind_for_device<'a>(
    ifaces: &'a [IfaceCand],
    device: Ipv4Addr,
) -> Option<&'a IfaceCand> {
    let mut hits: Vec<&IfaceCand> = ifaces
        .iter()
        .filter(|i| iface_role(&i.name, i.ip) == IfaceRole::Preferred)
        .filter(|i| ipv4_in_subnet(device, i.ip, i.prefix))
        .collect();
    if hits.is_empty() {
        return None;
    }
    hits.sort_by(|a, b| {
        b.prefix
            .cmp(&a.prefix)
            .then(prefix_rank(a.ip).cmp(&prefix_rank(b.ip)))
            .then(a.name.cmp(&b.name))
            .then(a.ip.cmp(&b.ip))
    });
    hits.first().copied()
}

fn prefix_of(mask: Ipv4Addr) -> u8 {
    let bits = u32::from(mask).count_ones() as u8;
    if bits == 0 || bits > 30 {
        24
    } else {
        bits
    }
}

pub fn ipv4_in_subnet(ip: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let prefix = prefix.clamp(1, 32);
    if prefix >= 32 {
        return ip == network;
    }
    let shift = 32 - prefix;
    (u32::from(ip) >> shift) == (u32::from(network) >> shift)
}

/// flume's idle `RecvTimeoutError::Timeout` displays as "timed out waiting on a channel".
/// That string does **not** contain "timeout", so a substring check for "timeout" treats every
/// quiet poll as a hard mDNS failure. Idle waits are not failures; a closed channel is.
pub fn is_mdns_idle_timeout(msg: &str) -> bool {
    let s = msg.to_ascii_lowercase();
    s.contains("timed out") || s.contains("timeout")
}

fn iface_label(name: &str, ip: Ipv4Addr) -> String {
    format!("{name} ({ip})")
}

struct LanSnapshot {
    bind: Option<IfaceCand>,
    preferred: Vec<IfaceCand>,
    browse: Vec<String>,
    skipped: Vec<String>,
    skipped_ips: Vec<Ipv4Addr>,
}

/// Stable description of the home-LAN adapters. The UI compares this and starts a
/// fresh mDNS browse when it changes. Discovery itself keeps no device cache.
pub fn network_fingerprint() -> String {
    let snap = snapshot_ifaces();
    fingerprint_of(&snap.preferred, snap.bind.as_ref().map(|b| b.ip))
}

fn fingerprint_of(preferred: &[IfaceCand], bind: Option<Ipv4Addr>) -> String {
    let mut parts: Vec<String> = preferred
        .iter()
        .map(|i| format!("{} {}/{}", i.name, i.ip, i.prefix))
        .collect();
    parts.sort();
    let bind_s = bind.map(|ip| ip.to_string()).unwrap_or_else(|| "-".into());
    format!("bind={bind_s}|{}", parts.join(","))
}

/// A resolved receiver is on the home LAN when it sits in any preferred adapter's
/// subnet. 2.4 GHz and 5 GHz on one router share that subnet; the band is not a filter.
fn device_on_home_lan(ip: Ipv4Addr, preferred: &[IfaceCand]) -> bool {
    if !is_safe_bind_ip(ip) {
        return false;
    }
    preferred.iter().any(|c| ipv4_in_subnet(ip, c.ip, c.prefix))
}

fn snapshot_ifaces() -> LanSnapshot {
    let addrs = if_addrs::get_if_addrs().unwrap_or_default();
    let mut ifaces = Vec::new();
    for i in addrs {
        let if_addrs::IfAddr::V4(v4) = i.addr else {
            continue;
        };
        ifaces.push(IfaceCand {
            name: i.name,
            ip: v4.ip,
            prefix: prefix_of(v4.netmask),
        });
    }
    let route = default_route_v4().ok();
    let bind = choose_bind_ip(&ifaces, route).cloned();
    let preferred: Vec<IfaceCand> = ifaces
        .iter()
        .filter(|i| iface_role(&i.name, i.ip) == IfaceRole::Preferred)
        .cloned()
        .collect();
    let mut browse = Vec::new();
    let mut skipped = Vec::new();
    let mut skipped_ips = Vec::new();
    for i in &ifaces {
        let label = iface_label(&i.name, i.ip);
        match iface_role(&i.name, i.ip) {
            IfaceRole::Preferred => browse.push(label),
            IfaceRole::Skip => {
                if !i.ip.is_loopback() && !i.ip.is_unspecified() {
                    skipped_ips.push(i.ip);
                    skipped.push(label);
                }
            }
        }
    }
    browse.sort();
    skipped.sort();
    LanSnapshot {
        bind,
        preferred,
        browse,
        skipped,
        skipped_ips,
    }
}

fn default_route_v4() -> Result<Ipv4Addr, String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.connect("8.8.8.8:80").map_err(|e| e.to_string())?;
    match sock.local_addr().map_err(|e| e.to_string())?.ip() {
        IpAddr::V4(v4) => Ok(v4),
        other => Err(other.to_string()),
    }
}

fn no_tv_diagnostic(snap: &LanSnapshot, timeout_ms: u64, extra: Option<&str>) -> (String, String) {
    let notice = cast_notice(CastNotice::NoTv, home_lan_category());
    let browse = if snap.browse.is_empty() {
        "none".to_string()
    } else {
        snap.browse.join(", ")
    };
    let skipped = if snap.skipped.is_empty() {
        String::new()
    } else {
        format!(" Skipped interfaces: {}.", snap.skipped.join(", "))
    };
    let extra = extra.map(|e| format!(" Detail: {e}.")).unwrap_or_default();
    let lan = if snap.bind.is_none() {
        " This PC has no home-network address (VPN or Tailscale may be the only route)."
    } else {
        ""
    };
    let diagnostic =
        format!("{notice}{lan} Looked for {timeout_ms} ms on [{browse}].{skipped}{extra}");
    (notice, diagnostic)
}

fn empty_discover(
    snap: &LanSnapshot,
    timeout_ms: u64,
    interfaces: Vec<String>,
    extra: Option<&str>,
) -> CastDiscoverResult {
    let (notice, diagnostic) = no_tv_diagnostic(snap, timeout_ms, extra);
    CastDiscoverResult {
        devices: vec![],
        timeout_ms,
        interfaces,
        error: Some(notice),
        diagnostic,
    }
}

/// Discover Cast receivers. Returns within the timeout (hard cap 10s, always under 15s).
/// An empty network is a normal result with [`NO_TV_ERROR`], not an mDNS channel failure.
pub fn discover_devices(timeout_ms: Option<u64>) -> Result<CastDiscoverResult, String> {
    let ms = timeout_ms
        .unwrap_or(DISCOVER_DEFAULT_MS)
        .min(DISCOVER_MAX_MS)
        .max(500);
    let snap = snapshot_ifaces();
    let interfaces = snap.browse.clone();

    if snap.bind.is_none() && snap.browse.is_empty() {
        eprintln!("[cast] discover: no home-network adapter");
        return Ok(empty_discover(&snap, ms, interfaces, None));
    }

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[cast] discover: mDNS daemon failed to start");
            return Ok(empty_discover(
                &snap,
                ms,
                interfaces,
                Some(&format!("mDNS could not start ({e})")),
            ));
        }
    };

    let mut setup_notes: Vec<String> = Vec::new();
    // Do not disable IfKind::All. That drops every socket; if the following enable
    // does not land, browse listens nowhere and the UI used to blame a channel timeout.
    if let Err(e) = daemon.disable_interface(IfKind::IPv6) {
        let msg = e.to_string();
        if !is_mdns_idle_timeout(&msg) {
            setup_notes.push(format!("ipv6 filter: {msg}"));
        }
    }
    for ip in &snap.skipped_ips {
        if let Err(e) = daemon.disable_interface(IfKind::Addr(IpAddr::V4(*ip))) {
            let msg = e.to_string();
            if !is_mdns_idle_timeout(&msg) {
                setup_notes.push("skipped-interface filter failed".into());
                break;
            }
        }
    }
    for cand in &snap.preferred {
        if let Err(e) = daemon.enable_interface(IfKind::Addr(IpAddr::V4(cand.ip))) {
            let msg = e.to_string();
            if !is_mdns_idle_timeout(&msg) {
                setup_notes.push("could not pin browse to a home-network adapter".into());
                break;
            }
        }
    }

    let receiver = match daemon.browse(CAST_SERVICE) {
        Ok(r) => r,
        Err(e) => {
            let _ = daemon.shutdown();
            eprintln!("[cast] discover: browse failed to start");
            return Ok(empty_discover(
                &snap,
                ms,
                interfaces,
                Some(&format!("mDNS browse failed ({e})")),
            ));
        }
    };

    // Give the multicast join a moment before the deadline math, inside the same cap.
    thread::sleep(Duration::from_millis(200));
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut found: HashMap<String, CastDeviceInfo> = HashMap::new();
    let mut real_error: Option<String> = None;

    while Instant::now() < deadline {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        let wait = remain.min(Duration::from_millis(250));
        match receiver.recv_timeout(wait) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                // Do not read the device `id` TXT property (that is the Cast UUID).
                let port = info.get_port();
                let name = info
                    .get_property_val_str("fn")
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Chromecast".to_string());
                let model = info
                    .get_property_val_str("md")
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty());
                for addr in info.get_addresses_v4() {
                    let ip = *addr;
                    if !device_on_home_lan(ip, &snap.preferred) {
                        continue;
                    }
                    let host = ip.to_string();
                    found.insert(
                        format!("{host}:{port}"),
                        CastDeviceInfo {
                            name: name.clone(),
                            host,
                            port,
                            model: model.clone(),
                        },
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if is_mdns_idle_timeout(&msg) {
                    continue;
                }
                real_error = Some("mDNS browse stopped".into());
                break;
            }
        }
    }

    if let Ok(done) = daemon.shutdown() {
        let _ = done.recv_timeout(Duration::from_millis(400));
    }

    let mut list: Vec<_> = found.into_values().collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let extra = real_error
        .as_deref()
        .or(setup_notes.first().map(String::as_str));
    let (error, diagnostic) = if list.is_empty() {
        let (notice, diagnostic) = no_tv_diagnostic(&snap, ms, extra);
        (Some(notice), diagnostic)
    } else {
        (
            None,
            format!("Found {} on the home network ({} ms).", list.len(), ms),
        )
    };
    eprintln!(
        "[cast] discover finished in ≤{ms} ms, {} device(s)",
        list.len()
    );
    Ok(CastDiscoverResult {
        devices: list,
        timeout_ms: ms,
        interfaces,
        error,
        diagnostic,
    })
}

pub fn parse_cast_host(host: &str, port: Option<u16>) -> Result<(Ipv4Addr, u16), String> {
    let ip: Ipv4Addr = host
        .trim()
        .parse()
        .map_err(|_| "Enter the TV's IPv4 address, for example 192.168.1.50.".to_string())?;
    if !is_safe_bind_ip(ip) {
        return Err(
            "That address is not on a home network. Cast stays on the LAN (no public IP, Tailscale, or link-local)."
                .into(),
        );
    }
    let port = port.unwrap_or(CAST_APP_ID_PORT);
    if port == 0 {
        return Err("Cast port must be between 1 and 65535.".into());
    }
    Ok((ip, port))
}

fn file_stem_label(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string()
}

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// JPEG, PNG, or MP4 (H.264 or HEVC + AAC). Anything else is refused before it
/// is tokenized. TV limits (level, frame size, frame rate, bit depth, bitrate)
/// are applied when the file is cast, not here.
pub fn classify_playlist_file(path: &Path) -> Result<(&'static str, String), String> {
    let canon = path
        .canonicalize()
        .map_err(|_| format!("{} is not available", file_stem_label(path)))?;
    if !canon.is_file() {
        return Err(format!("{} is not a file", file_stem_label(path)));
    }
    let ext = ext_of(&canon);
    let label = file_stem_label(&canon);
    match ext.as_str() {
        "jpg" | "jpeg" => {
            sniff_still(&canon, true)?;
            Ok(("image/jpeg", "still".into()))
        }
        "png" => {
            sniff_still(&canon, false)?;
            Ok(("image/png", "still".into()))
        }
        "mp4" | "m4v" => {
            if ext == "m4v" {
                return Err(format!("{label}: {UNSUPPORTED_MEDIA}"));
            }
            if !mp4_is_tv_video(&canon).unwrap_or(false) {
                return Err(format!("{label}: {UNSUPPORTED_MEDIA}"));
            }
            Ok(("video/mp4", "video".into()))
        }
        _ => Err(format!("{label}: {UNSUPPORTED_MEDIA}")),
    }
}

fn sniff_still(path: &Path, jpeg: bool) -> Result<(), String> {
    let mut f =
        File::open(path).map_err(|_| format!("{} is not readable", file_stem_label(path)))?;
    let mut buf = [0u8; 8];
    let n = f.read(&mut buf).unwrap_or(0);
    let ok = if jpeg {
        n >= 3 && buf[0] == 0xff && buf[1] == 0xd8 && buf[2] == 0xff
    } else {
        n >= 8 && buf[..8] == *b"\x89PNG\r\n\x1a\n"
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{}: {UNSUPPORTED_MEDIA}", file_stem_label(path)))
    }
}

fn read_u32(f: &mut File) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64(f: &mut File) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// Walk MP4 boxes without reading `mdat`. True only when an `avc1`/`avc3` sample and an `mp4a` sample both exist.
pub fn mp4_is_h264_aac(path: &Path) -> std::io::Result<bool> {
    mp4_has_codecs(path, false)
}

/// H.264 (`avc1`/`avc3`) or HEVC (`hvc1`/`hev1`) plus AAC. `hev1` is registered
/// so cast time can name that tag; the TV limit check still skips it.
fn mp4_is_tv_video(path: &Path) -> std::io::Result<bool> {
    mp4_has_codecs(path, true)
}

fn mp4_has_codecs(path: &Path, hevc: bool) -> std::io::Result<bool> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut saw_visual = false;
    let mut saw_aac = false;
    walk_boxes(&mut f, 0, len, 0, &mut saw_visual, &mut saw_aac, hevc)?;
    Ok(saw_visual && saw_aac)
}

fn walk_boxes(
    f: &mut File,
    start: u64,
    end: u64,
    depth: u8,
    saw_visual: &mut bool,
    saw_aac: &mut bool,
    hevc: bool,
) -> std::io::Result<()> {
    if depth > 8 || *saw_visual && *saw_aac {
        return Ok(());
    }
    f.seek(SeekFrom::Start(start))?;
    while f.stream_position()? + 8 <= end {
        let pos = f.stream_position()?;
        let size32 = read_u32(f)? as u64;
        let mut typ = [0u8; 4];
        f.read_exact(&mut typ)?;
        let (size, header) = if size32 == 1 {
            let large = read_u64(f)?;
            (large, 16u64)
        } else if size32 == 0 {
            (end.saturating_sub(pos), 8u64)
        } else {
            (size32, 8u64)
        };
        if size < header {
            return Ok(());
        }
        let box_end = pos.saturating_add(size);
        if box_end > end || box_end <= pos {
            return Ok(());
        }
        if &typ == b"stsd" {
            parse_stsd(f, box_end, saw_visual, saw_aac, hevc)?;
        } else if matches!(&typ, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl") {
            let child = f.stream_position()?;
            walk_boxes(f, child, box_end, depth + 1, saw_visual, saw_aac, hevc)?;
        }
        f.seek(SeekFrom::Start(box_end))?;
        if *saw_visual && *saw_aac {
            return Ok(());
        }
    }
    Ok(())
}

fn parse_stsd(
    f: &mut File,
    box_end: u64,
    saw_visual: &mut bool,
    saw_aac: &mut bool,
    hevc: bool,
) -> std::io::Result<()> {
    if f.stream_position()? + 8 > box_end {
        return Ok(());
    }
    let _ver_flags = read_u32(f)?;
    let count = read_u32(f)?.min(32);
    for _ in 0..count {
        if f.stream_position()? + 8 > box_end {
            break;
        }
        let entry_pos = f.stream_position()?;
        let entry_size = read_u32(f)? as u64;
        let mut four = [0u8; 4];
        f.read_exact(&mut four)?;
        match &four {
            b"avc1" | b"avc3" => *saw_visual = true,
            b"hvc1" | b"hev1" if hevc => *saw_visual = true,
            b"mp4a" => *saw_aac = true,
            _ => {}
        }
        let entry_end = if entry_size >= 8 {
            entry_pos.saturating_add(entry_size)
        } else {
            break;
        };
        if entry_end > box_end {
            break;
        }
        f.seek(SeekFrom::Start(entry_end))?;
    }
    Ok(())
}

/// Highest H.264/HEVC level SlideX will cast, in tenths (52 = level 5.2).
const CAST_MAX_VIDEO_LEVEL: u8 = 52;

/// H.264 Table A-1, in tenths: level, MaxFS, MaxMBPS. Used for a missing level
/// on both H.264 and HEVC (16×16 macroblocks, rounded up).
const H264_LEVEL_TABLE: &[(u8, u32, u32)] = &[
    (10, 99, 1_485),
    (11, 396, 3_000),
    (12, 396, 6_000),
    (13, 396, 11_880),
    (20, 396, 11_880),
    (21, 792, 19_800),
    (22, 1_620, 20_250),
    (30, 1_620, 40_500),
    (31, 3_600, 108_000),
    (32, 5_120, 216_000),
    (40, 8_192, 245_760),
    (41, 8_192, 245_760),
    (42, 8_704, 522_240),
    (50, 22_080, 589_824),
    (51, 36_864, 983_040),
    (52, 36_864, 2_073_600),
    (60, 139_264, 4_177_920),
    (61, 139_264, 8_355_840),
    (62, 139_264, 16_711_680),
];

const UHD_W: u32 = 3840;
const UHD_H: u32 = 2160;
const FHD_W: u32 = 1920;
const FHD_H: u32 = 1080;
const BITRATE_FHD: u64 = 20_000_000;
const BITRATE_UHD: u64 = 40_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisualCodec {
    Avc,
    Hvc1,
    Hev1,
}

struct TrackProbe {
    width: u32,
    height: u32,
    timescale: u32,
    duration: u64,
    min_delta: u32,
    codec: Option<VisualCodec>,
    /// Tenths from avcC, or the HEVC general_level_idc converted to tenths.
    /// `Some(0)` means the box was present and the level byte was 0.
    level: Option<u8>,
    /// `bit_depth_luma_minus8`. Missing when the file does not say.
    bit_depth: Option<u8>,
    bitrate: Option<u64>,
}

impl TrackProbe {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            timescale: 0,
            duration: 0,
            min_delta: 0,
            codec: None,
            level: None,
            bit_depth: None,
            bitrate: None,
        }
    }
}

struct CastVideoDecision {
    /// Sentence shown to the user. `None` means the file may be cast.
    skip: Option<String>,
    /// Set only when avcC/hvcC had no level (missing box or 0). Format `5.1`.
    derived: Option<String>,
}

struct Mp4Box {
    typ: [u8; 4],
    payload: u64,
    end: u64,
}

/// SlideX does not ship ffmpeg. Limits come from the MP4 boxes (`avcC` /
/// `hvcC`, `tkhd`, `mdhd`, `stts`, `btrt`). A value the file does not carry
/// is not treated as a failure, so a stub sample entry still loads.
fn decide_cast_video(path: &Path) -> CastVideoDecision {
    let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let probe = match read_mp4_video(path) {
        Ok(probe) => probe,
        Err(_) => {
            return CastVideoDecision {
                skip: None,
                derived: None,
            };
        }
    };
    let mut derived = None;
    let mut level = probe.level.filter(|tenths| *tenths != 0);
    if level.is_none() {
        if let Some(tenths) =
            derive_h264_level(probe.width, probe.height, probe.timescale, probe.min_delta)
        {
            derived = Some(format_level(tenths));
            level = Some(tenths);
        }
    }
    let mut parts = Vec::new();
    if probe.codec == Some(VisualCodec::Hev1) {
        parts.push("HEVC (hev1)".to_string());
    }
    if let Some(minus8) = probe.bit_depth {
        if minus8 != 0 {
            parts.push(format!("{}-bit", 8u32.saturating_add(minus8 as u32)));
        }
    }
    if probe.timescale > 0 && probe.min_delta > 0 && fps_over_60(probe.timescale, probe.min_delta) {
        parts.push(format_fps(probe.timescale, probe.min_delta));
    }
    let have_size = probe.width > 0 && probe.height > 0;
    if have_size && !fits_box(probe.width, probe.height, UHD_W, UHD_H) {
        parts.push(format!("{}x{}", probe.width, probe.height));
    }
    let bitrate = probe
        .bitrate
        .or_else(|| bitrate_from_file(file_len, probe.timescale, probe.duration));
    if let (Some(rate), true) = (bitrate, have_size) {
        let above_fhd = !fits_box(probe.width, probe.height, FHD_W, FHD_H);
        let cap = if above_fhd { BITRATE_UHD } else { BITRATE_FHD };
        if rate > cap {
            parts.push(format_mbit(rate, cap));
        }
    }
    let level_sentence = level.and_then(|tenths| {
        if tenths > CAST_MAX_VIDEO_LEVEL {
            let name = match probe.codec {
                Some(VisualCodec::Hvc1) | Some(VisualCodec::Hev1) => "HEVC",
                _ => "H.264",
            };
            Some(format!(
                "{name} level {} may not play on this TV",
                format_level(tenths)
            ))
        } else {
            None
        }
    });
    CastVideoDecision {
        skip: skip_sentence(&parts, level_sentence),
        derived,
    }
}

fn join_skip_notes(notes: &[String]) -> Option<String> {
    if notes.is_empty() {
        None
    } else {
        Some(notes.join(". "))
    }
}

fn skip_sentence(parts: &[String], level_sentence: Option<String>) -> Option<String> {
    if parts.is_empty() && level_sentence.is_none() {
        return None;
    }
    let mut out = String::new();
    if !parts.is_empty() {
        let verb = if parts.len() == 1 { "isn't" } else { "aren't" };
        out = format!("{} {verb} supported by the TV", join_limits(parts));
    }
    if let Some(level) = level_sentence {
        if out.is_empty() {
            out = level;
        } else {
            out.push_str(". ");
            out.push_str(&level);
        }
    }
    Some(out)
}

fn join_limits(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        _ => {
            let (last, rest) = parts.split_last().unwrap();
            format!("{} and {last}", rest.join(", "))
        }
    }
}

fn format_level(tenths: u8) -> String {
    format!("{}.{}", tenths / 10, tenths % 10)
}

fn fits_box(w: u32, h: u32, box_w: u32, box_h: u32) -> bool {
    (w <= box_w && h <= box_h) || (w <= box_h && h <= box_w)
}

fn fps_over_60(timescale: u32, delta: u32) -> bool {
    delta > 0 && (timescale as u64) > 60u64.saturating_mul(delta as u64)
}

fn format_fps(timescale: u32, delta: u32) -> String {
    if delta == 0 {
        return "0 fps".to_string();
    }
    if timescale % delta == 0 {
        return format!("{} fps", timescale / delta);
    }
    let tenths = (timescale as u64)
        .saturating_mul(10)
        .saturating_add((delta as u64) / 2)
        / delta as u64;
    format!("{}.{} fps", tenths / 10, tenths % 10)
}

/// Nearest Mbit/s. One decimal when the integer would hide a value over `cap`.
fn format_mbit(bitrate: u64, cap: u64) -> String {
    let nearest = bitrate.saturating_add(500_000) / 1_000_000;
    let cap_mbit = cap / 1_000_000;
    if bitrate > cap && nearest <= cap_mbit {
        let tenths = bitrate.saturating_add(50_000) / 100_000;
        return format!("{}.{} Mbit/s", tenths / 10, tenths % 10);
    }
    format!("{nearest} Mbit/s")
}

fn bitrate_from_file(len: u64, timescale: u32, duration: u64) -> Option<u64> {
    if timescale == 0 || duration == 0 || len == 0 {
        return None;
    }
    let bits = (len as u128)
        .saturating_mul(8)
        .saturating_mul(timescale as u128);
    let rate = bits / duration as u128;
    if rate == 0 {
        None
    } else {
        Some(rate.min(u64::MAX as u128) as u64)
    }
}

/// Lowest Table A-1 level whose MaxFS and MaxMBPS both hold.
/// `fs * timescale <= MaxMBPS * sample_delta` avoids a floating frame rate.
fn derive_h264_level(width: u32, height: u32, timescale: u32, delta: u32) -> Option<u8> {
    if width == 0 || height == 0 || timescale == 0 || delta == 0 {
        return None;
    }
    let mb_w = (width as u64).saturating_add(15) / 16;
    let mb_h = (height as u64).saturating_add(15) / 16;
    let fs = mb_w.saturating_mul(mb_h);
    let ts = timescale as u64;
    let sample_delta = delta as u64;
    for &(level, max_fs, max_mbps) in H264_LEVEL_TABLE {
        if fs <= max_fs as u64
            && fs.saturating_mul(ts) <= (max_mbps as u64).saturating_mul(sample_delta)
        {
            return Some(level);
        }
    }
    Some(62)
}

fn hevc_level_tenths(idc: u8) -> u8 {
    let major = idc / 30;
    let minor = (idc % 30) / 3;
    major.saturating_mul(10).saturating_add(minor)
}

fn read_mp4_video(path: &Path) -> std::io::Result<TrackProbe> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut probe = TrackProbe::new();
    walk_probe(&mut f, 0, len, 0, &mut probe)?;
    Ok(probe)
}

fn read_box_header(f: &mut File, end: u64) -> std::io::Result<Option<Mp4Box>> {
    if f.stream_position()? + 8 > end {
        return Ok(None);
    }
    let pos = f.stream_position()?;
    let size32 = read_u32(f)? as u64;
    let mut typ = [0u8; 4];
    f.read_exact(&mut typ)?;
    let (size, header) = if size32 == 1 {
        if f.stream_position()? + 8 > end {
            return Ok(None);
        }
        (read_u64(f)?, 16u64)
    } else if size32 == 0 {
        (end.saturating_sub(pos), 8u64)
    } else {
        (size32, 8u64)
    };
    if size < header {
        return Ok(None);
    }
    let box_end = pos.saturating_add(size);
    if box_end > end || box_end <= pos {
        return Ok(None);
    }
    Ok(Some(Mp4Box {
        typ,
        payload: pos + header,
        end: box_end,
    }))
}

fn walk_probe(
    f: &mut File,
    start: u64,
    end: u64,
    depth: u8,
    probe: &mut TrackProbe,
) -> std::io::Result<()> {
    if depth > 8 {
        return Ok(());
    }
    f.seek(SeekFrom::Start(start))?;
    while f.stream_position()? + 8 <= end {
        let Some(bx) = read_box_header(f, end)? else {
            break;
        };
        match &bx.typ {
            b"trak" => {
                let mut inner = TrackProbe::new();
                walk_probe(f, bx.payload, bx.end, depth + 1, &mut inner)?;
                if inner.codec.is_some() {
                    *probe = inner;
                }
            }
            b"moov" | b"mdia" | b"minf" | b"stbl" => {
                walk_probe(f, bx.payload, bx.end, depth + 1, probe)?;
            }
            b"tkhd" => parse_tkhd(f, bx.payload, bx.end, probe)?,
            b"mdhd" => parse_mdhd(f, bx.payload, bx.end, probe)?,
            b"stts" => parse_stts(f, bx.payload, bx.end, probe)?,
            b"stsd" => parse_visual_stsd(f, bx.payload, bx.end, probe)?,
            _ => {}
        }
        f.seek(SeekFrom::Start(bx.end))?;
    }
    Ok(())
}

fn parse_tkhd(f: &mut File, payload: u64, end: u64, probe: &mut TrackProbe) -> std::io::Result<()> {
    if payload + 1 > end {
        return Ok(());
    }
    f.seek(SeekFrom::Start(payload))?;
    let mut ver = [0u8; 1];
    f.read_exact(&mut ver)?;
    let width_at = payload + if ver[0] == 0 { 76 } else { 88 };
    if width_at + 8 > end {
        return Ok(());
    }
    f.seek(SeekFrom::Start(width_at))?;
    let width = read_u32(f)? >> 16;
    let height = read_u32(f)? >> 16;
    if width > 0 && height > 0 {
        probe.width = width;
        probe.height = height;
    }
    Ok(())
}

fn parse_mdhd(f: &mut File, payload: u64, end: u64, probe: &mut TrackProbe) -> std::io::Result<()> {
    if payload + 1 > end {
        return Ok(());
    }
    f.seek(SeekFrom::Start(payload))?;
    let mut ver = [0u8; 1];
    f.read_exact(&mut ver)?;
    if ver[0] == 0 {
        if payload + 20 > end {
            return Ok(());
        }
        f.seek(SeekFrom::Start(payload + 12))?;
        probe.timescale = read_u32(f)?;
        probe.duration = read_u32(f)? as u64;
    } else {
        if payload + 32 > end {
            return Ok(());
        }
        f.seek(SeekFrom::Start(payload + 20))?;
        probe.timescale = read_u32(f)?;
        probe.duration = read_u64(f)?;
    }
    Ok(())
}

fn parse_stts(f: &mut File, payload: u64, end: u64, probe: &mut TrackProbe) -> std::io::Result<()> {
    if payload + 8 > end {
        return Ok(());
    }
    f.seek(SeekFrom::Start(payload + 4))?;
    let count = read_u32(f)?.min(4096);
    let mut min_delta = u32::MAX;
    for _ in 0..count {
        if f.stream_position()? + 8 > end {
            break;
        }
        let _samples = read_u32(f)?;
        let delta = read_u32(f)?;
        if delta > 0 && delta < min_delta {
            min_delta = delta;
        }
    }
    if min_delta != u32::MAX {
        probe.min_delta = min_delta;
    }
    Ok(())
}

fn parse_visual_stsd(
    f: &mut File,
    payload: u64,
    end: u64,
    probe: &mut TrackProbe,
) -> std::io::Result<()> {
    if payload + 8 > end {
        return Ok(());
    }
    f.seek(SeekFrom::Start(payload))?;
    let _ver_flags = read_u32(f)?;
    let count = read_u32(f)?.min(32);
    for _ in 0..count {
        if f.stream_position()? + 8 > end {
            break;
        }
        let entry_pos = f.stream_position()?;
        let entry_size = read_u32(f)? as u64;
        let mut four = [0u8; 4];
        f.read_exact(&mut four)?;
        let entry_end = if entry_size >= 8 {
            entry_pos.saturating_add(entry_size)
        } else {
            break;
        };
        if entry_end > end {
            break;
        }
        let codec = match &four {
            b"avc1" | b"avc3" => Some(VisualCodec::Avc),
            b"hvc1" => Some(VisualCodec::Hvc1),
            b"hev1" => Some(VisualCodec::Hev1),
            _ => None,
        };
        if let Some(codec) = codec {
            probe.codec = Some(codec);
            // VisualSampleEntry fields are 78 bytes after the 8-byte box header.
            let child = entry_pos.saturating_add(86);
            if child < entry_end {
                walk_sample_children(f, child, entry_end, probe)?;
            }
        }
        f.seek(SeekFrom::Start(entry_end))?;
    }
    Ok(())
}

fn walk_sample_children(
    f: &mut File,
    start: u64,
    end: u64,
    probe: &mut TrackProbe,
) -> std::io::Result<()> {
    f.seek(SeekFrom::Start(start))?;
    while f.stream_position()? + 8 <= end {
        let Some(bx) = read_box_header(f, end)? else {
            break;
        };
        match &bx.typ {
            b"avcC" => parse_avcc(f, bx.payload, bx.end, probe)?,
            b"hvcC" => parse_hvcc(f, bx.payload, bx.end, probe)?,
            b"btrt" => parse_btrt(f, bx.payload, bx.end, probe)?,
            _ => {}
        }
        f.seek(SeekFrom::Start(bx.end))?;
    }
    Ok(())
}

fn read_span(f: &mut File, payload: u64, end: u64, cap: usize) -> std::io::Result<Vec<u8>> {
    let n = ((end.saturating_sub(payload)) as usize).min(cap);
    let mut buf = vec![0u8; n];
    if n > 0 {
        f.seek(SeekFrom::Start(payload))?;
        f.read_exact(&mut buf)?;
    }
    Ok(buf)
}

fn parse_avcc(f: &mut File, payload: u64, end: u64, probe: &mut TrackProbe) -> std::io::Result<()> {
    let buf = read_span(f, payload, end, 512)?;
    if buf.len() < 4 {
        return Ok(());
    }
    let profile = buf[1];
    probe.level = Some(buf[3]);
    if matches!(profile, 66 | 77 | 88) {
        probe.bit_depth = Some(0);
        return Ok(());
    }
    if let Some(depth) = avcc_sps_bit_depth(&buf) {
        probe.bit_depth = Some(depth);
    }
    Ok(())
}

fn avcc_sps_bit_depth(buf: &[u8]) -> Option<u8> {
    if buf.len() < 6 {
        return None;
    }
    let num_sps = (buf[5] & 0x1f) as usize;
    if num_sps == 0 {
        return None;
    }
    let mut off = 6usize;
    if off + 2 > buf.len() {
        return None;
    }
    let sps_len = u16::from_be_bytes([buf[off], buf[off + 1]]) as usize;
    off += 2;
    if sps_len < 2 || off + sps_len > buf.len() {
        return None;
    }
    sps_bit_depth(&buf[off..off + sps_len])
}

fn parse_hvcc(f: &mut File, payload: u64, end: u64, probe: &mut TrackProbe) -> std::io::Result<()> {
    let buf = read_span(f, payload, end, 64)?;
    // general_level_idc is byte 12. A shorter box has no level.
    if buf.len() < 13 {
        return Ok(());
    }
    probe.level = Some(hevc_level_tenths(buf[12]));
    if buf.len() > 17 {
        probe.bit_depth = Some(buf[17] & 0x07);
    }
    Ok(())
}

fn parse_btrt(f: &mut File, payload: u64, end: u64, probe: &mut TrackProbe) -> std::io::Result<()> {
    let buf = read_span(f, payload, end, 12)?;
    if buf.len() < 12 {
        return Ok(());
    }
    let max_rate = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let avg_rate = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if avg_rate != 0 {
        probe.bitrate = Some(avg_rate as u64);
    } else if max_rate != 0 {
        probe.bitrate = Some(max_rate as u64);
    }
    Ok(())
}

fn ebsp_to_rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut i = 0;
    while i < ebsp.len() {
        if i + 2 < ebsp.len() && ebsp[i] == 0 && ebsp[i + 1] == 0 && ebsp[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(ebsp[i]);
            i += 1;
        }
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let i = self.bit / 8;
        if i >= self.data.len() {
            return None;
        }
        let shift = 7 - (self.bit % 8);
        self.bit += 1;
        Some((self.data[i] >> shift) & 1)
    }

    fn read_bits(&mut self, n: u32) -> Option<u32> {
        if n > 31 {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as u32;
        }
        Some(v)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        loop {
            if self.read_bit()? == 1 {
                break;
            }
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.read_bits(zeros)?;
        Some(((1u32 << zeros) - 1).saturating_add(rest))
    }
}

fn sps_bit_depth(nal: &[u8]) -> Option<u8> {
    if nal.is_empty() {
        return None;
    }
    let rbsp = ebsp_to_rbsp(nal);
    if rbsp.len() < 4 {
        return None;
    }
    let mut bits = BitReader::new(&rbsp[1..]);
    let profile = bits.read_bits(8)? as u8;
    let _constraint = bits.read_bits(8)?;
    let _level = bits.read_bits(8)?;
    let _sps_id = bits.read_ue()?;
    const HIGH: &[u8] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];
    if !HIGH.contains(&profile) {
        return Some(0);
    }
    let chroma = bits.read_ue()?;
    if chroma == 3 {
        let _separate = bits.read_bits(1)?;
    }
    let luma = bits.read_ue()?;
    if luma > 6 {
        return None;
    }
    Some(luma as u8)
}

#[derive(Clone, Debug)]
struct AllowEntry {
    path: PathBuf,
    content_type: String,
    kind: String,
    /// 1-based index in the path list the page sent.
    page_index: u32,
}

struct AllowBook {
    token: String,
    files: HashMap<String, AllowEntry>,
    by_path: HashMap<PathBuf, String>,
    /// Playlist order. Auto-advance walks this; the map alone has no order.
    order: Vec<PathBuf>,
}

impl AllowBook {
    fn new() -> Self {
        Self {
            token: new_token(),
            files: HashMap::new(),
            by_path: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn revoke(&mut self) {
        self.files.clear();
        self.by_path.clear();
        self.order.clear();
        self.token = new_token();
    }
}

fn new_token() -> String {
    Uuid::new_v4().simple().to_string()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, PartialEq, Eq)]
enum Deny {
    NotFound,
}

/// Map `/m/<token>/<key>` to a playlist file. Rejects traversal, listing, and unknown tokens.
fn resolve_media_url<'a>(url: &str, book: &'a AllowBook) -> Result<&'a AllowEntry, Deny> {
    let bare = url.split(['?', '#']).next().unwrap_or(url);
    if bare.contains('\\') || bare.contains('%') || bare.contains("..") || bare.contains("//") {
        return Err(Deny::NotFound);
    }
    let mut parts = bare.split('/').filter(|s| !s.is_empty());
    let root = parts.next();
    let token = parts.next();
    let key = parts.next();
    let extra = parts.next();
    if root != Some("m") || extra.is_some() {
        return Err(Deny::NotFound);
    }
    let (Some(token), Some(key)) = (token, key) else {
        return Err(Deny::NotFound);
    };
    if token.len() != book.token.len() || !ct_eq(token.as_bytes(), book.token.as_bytes()) {
        return Err(Deny::NotFound);
    }
    if key.len() > 80 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Deny::NotFound);
    }
    book.files.get(key).ok_or(Deny::NotFound)
}

fn replace_playlist(book: &mut AllowBook, paths: &[PathBuf]) -> PlaylistUpdate {
    book.files.clear();
    book.by_path.clear();
    book.order.clear();
    let mut skipped = Vec::new();
    let mut page_index = Vec::new();
    for (idx, path) in paths.iter().enumerate() {
        let page = (idx as u32).saturating_add(1);
        match classify_playlist_file(path) {
            Ok((ctype, kind)) => {
                let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
                if book.by_path.contains_key(&canon) {
                    continue;
                }
                let key = new_token();
                book.by_path.insert(canon.clone(), key.clone());
                book.order.push(canon.clone());
                page_index.push(page);
                book.files.insert(
                    key,
                    AllowEntry {
                        path: canon,
                        content_type: ctype.to_string(),
                        kind,
                        page_index: page,
                    },
                );
            }
            Err(reason) => skipped.push(reason),
        }
    }
    PlaylistUpdate {
        registered: book.files.len(),
        skipped,
        page_index,
    }
}

fn lookup_playlist<'a>(book: &'a AllowBook, path: &Path) -> Result<&'a AllowEntry, String> {
    let canon = path
        .canonicalize()
        .map_err(|_| "That slide is not in the current playlist.".to_string())?;
    let key = book
        .by_path
        .get(&canon)
        .ok_or_else(|| "That slide is not in the current playlist.".to_string())?;
    book.files
        .get(key)
        .ok_or_else(|| "That slide is not in the current playlist.".to_string())
}

struct MediaHttp {
    port: u16,
    bind_ip: Ipv4Addr,
    book: Arc<Mutex<AllowBook>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    inflight: Arc<AtomicUsize>,
}

impl MediaHttp {
    fn start_lan(ip: Ipv4Addr) -> Result<Self, String> {
        if !is_safe_bind_ip(ip) {
            return Err(
                "Refusing to listen: Cast only binds a home-network address, never 0.0.0.0, a public IP, or Tailscale."
                    .into(),
            );
        }
        Self::bind_on(ip, CAST_PORT_LO..=CAST_PORT_HI)
    }

    fn bind_on(ip: Ipv4Addr, ports: impl IntoIterator<Item = u16>) -> Result<Self, String> {
        Self::bind_with_idle(ip, ports, HTTP_IDLE)
    }

    fn bind_with_idle(
        ip: Ipv4Addr,
        ports: impl IntoIterator<Item = u16>,
        idle: Duration,
    ) -> Result<Self, String> {
        if ip.is_unspecified() {
            return Err("Refusing to listen on 0.0.0.0.".into());
        }
        let mut last = "no port in the Cast range was free".to_string();
        for port in ports {
            match TcpListener::bind(SocketAddr::from((ip, port))) {
                Ok(listener) => return Self::from_listener(listener, ip, idle),
                Err(e) => last = e.to_string(),
            }
        }
        Err(format!(
            "Could not bind the Cast media server on {ip} ports {CAST_PORT_LO}-{CAST_PORT_HI} ({last})."
        ))
    }

    fn from_listener(listener: TcpListener, ip: Ipv4Addr, idle: Duration) -> Result<Self, String> {
        let bound = listener
            .local_addr()
            .map_err(|e| format!("http local_addr: {e}"))?;
        let port = bound.port();
        let bound_ip = match bound.ip() {
            IpAddr::V4(v4) => v4,
            _ => return Err("Refusing to listen on a non-IPv4 address.".into()),
        };
        if bound_ip.is_unspecified() {
            return Err("Refusing to listen on 0.0.0.0.".into());
        }
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("http listen: {e}"))?;
        let book = Arc::new(Mutex::new(AllowBook::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let workers = Arc::new(Mutex::new(Vec::new()));
        let inflight = Arc::new(AtomicUsize::new(0));
        let book_t = Arc::clone(&book);
        let stop_t = Arc::clone(&stop);
        let workers_t = Arc::clone(&workers);
        let inflight_t = Arc::clone(&inflight);
        // The accept thread and its workers must see the test log directory.
        // CAST_LOG_OVERRIDE is thread-local, so copy it onto those threads.
        let log_dir = cast_log_override_dir();
        let join = thread::spawn(move || {
            apply_cast_log_dir(log_dir);
            http_loop(listener, book_t, stop_t, workers_t, inflight_t, idle);
        });
        cast_log(
            "media_listen",
            &format!("addr={bound_ip}:{port} pid={}", std::process::id()),
        );
        eprintln!("[cast] media server listening on one LAN address, port {port}");
        Ok(Self {
            port,
            bind_ip: ip,
            book,
            stop,
            join: Some(join),
            workers,
            inflight,
        })
    }

    fn set_playlist(&self, paths: &[PathBuf]) -> Result<PlaylistUpdate, String> {
        let mut book = self.book.lock().map_err(|_| "playlist lock".to_string())?;
        Ok(replace_playlist(&mut book, paths))
    }

    fn media_url(&self, path: &Path) -> Result<(String, String), String> {
        let book = self.book.lock().map_err(|_| "playlist lock".to_string())?;
        let entry = lookup_playlist(&book, path)?;
        let key = book
            .by_path
            .get(&entry.path)
            .ok_or_else(|| "That slide is not in the current playlist.".to_string())?;
        let url = format!(
            "http://{}:{}/m/{}/{}",
            self.bind_ip, self.port, book.token, key
        );
        Ok((url, entry.content_type.clone()))
    }

    fn kind_of(&self, path: &Path) -> Result<String, String> {
        let book = self.book.lock().map_err(|_| "playlist lock".to_string())?;
        Ok(lookup_playlist(&book, path)?.kind.clone())
    }

    /// 1-based page index of `path` in the list the page sent.
    fn index_of(&self, path: &Path) -> Result<u32, String> {
        let book = self.book.lock().map_err(|_| "playlist lock".to_string())?;
        Ok(lookup_playlist(&book, path)?.page_index)
    }

    /// The next castable slide after `item_index` (a page index), wrapping to
    /// the first. Skipped files keep their page slot, so the index does not compact.
    fn successor(&self, item_index: u32) -> Result<Successor, String> {
        let book = self.book.lock().map_err(|_| "playlist lock".to_string())?;
        if book.order.is_empty() {
            return Err("empty playlist".into());
        }
        if item_index == 0 {
            return Err("current item unknown".into());
        }
        let mut ranked: Vec<(u32, String)> = Vec::new();
        for path in &book.order {
            let key = book
                .by_path
                .get(path)
                .ok_or_else(|| "current item unknown".to_string())?;
            let entry = book
                .files
                .get(key)
                .ok_or_else(|| "current item unknown".to_string())?;
            ranked.push((entry.page_index, key.clone()));
        }
        ranked.sort_by_key(|(page, _)| *page);
        let next = ranked
            .iter()
            .find(|(page, _)| *page > item_index)
            .or_else(|| ranked.first())
            .ok_or_else(|| "current item unknown".to_string())?;
        let entry = book
            .files
            .get(&next.1)
            .ok_or_else(|| "current item unknown".to_string())?;
        let url = format!(
            "http://{}:{}/m/{}/{}",
            self.bind_ip, self.port, book.token, next.1
        );
        Ok(Successor {
            url,
            content_type: entry.content_type.clone(),
            item: next.0,
            media_type: media_type_label(&entry.kind),
        })
    }

    /// The next item a FINISHED video should LOAD. Videos that break a TV
    /// limit are logged and passed over with no wait. The walk stops when it
    /// returns to `item_index`.
    fn next_castable(&self, item_index: u32) -> (Result<Successor, &'static str>, Option<String>) {
        let (token, steps) = {
            let book = match self.book.lock() {
                Ok(book) => book,
                Err(_) => return (Err("playlist lock"), None),
            };
            if book.order.is_empty() {
                return (Err("empty playlist"), None);
            }
            if item_index == 0 {
                return (Err("current item unknown"), None);
            }
            let mut ranked: Vec<(u32, String)> = Vec::new();
            for path in &book.order {
                let Some(key) = book.by_path.get(path) else {
                    return (Err("current item unknown"), None);
                };
                let Some(entry) = book.files.get(key) else {
                    return (Err("current item unknown"), None);
                };
                ranked.push((entry.page_index, key.clone()));
            }
            ranked.sort_by_key(|(page, _)| *page);
            let mut steps = Vec::with_capacity(ranked.len());
            for (page, key) in ranked {
                let Some(entry) = book.files.get(&key) else {
                    return (Err("current item unknown"), None);
                };
                steps.push((
                    page,
                    key,
                    entry.path.clone(),
                    entry.content_type.clone(),
                    entry.kind.clone(),
                ));
            }
            (book.token.clone(), steps)
        };
        let start = steps
            .iter()
            .position(|(page, ..)| *page > item_index)
            .unwrap_or(0);
        let mut seen = HashSet::new();
        seen.insert(item_index);
        let mut skipped = Vec::new();
        let n = steps.len();
        for step in 0..n {
            let (page, key, path, content_type, kind) = &steps[(start + step) % n];
            if !seen.insert(*page) {
                break;
            }
            if kind == "video" {
                let decision = decide_cast_video(path);
                if let Some(level) = decision.derived.as_deref() {
                    cast_log(
                        "level_derived",
                        &format!("item={} level_derived={level}", item_log_token(*page)),
                    );
                }
                if let Some(reasons) = decision.skip {
                    cast_log(
                        "video_skipped",
                        &format!("item={} reasons={reasons}", item_log_token(*page)),
                    );
                    skipped.push(reasons);
                    continue;
                }
            }
            let url = format!("http://{}:{}/m/{}/{}", self.bind_ip, self.port, token, key);
            let note = join_skip_notes(&skipped);
            return (
                Ok(Successor {
                    url,
                    content_type: content_type.clone(),
                    item: *page,
                    media_type: media_type_label(kind),
                }),
                note,
            );
        }
        (Err("nothing castable"), join_skip_notes(&skipped))
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _busy = self.inflight.load(Ordering::SeqCst);
        if let Ok(mut book) = self.book.lock() {
            book.revoke();
        }
        // Join the accept thread first so it drops the listener and stops spawning.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let workers = self
            .workers
            .lock()
            .ok()
            .map(|mut w| std::mem::take(&mut *w))
            .unwrap_or_default();
        for worker in workers {
            let _ = worker.join();
        }
        eprintln!("[cast] media server stopped and tokens revoked");
    }
}

impl Drop for MediaHttp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn http_loop(
    listener: TcpListener,
    book: Arc<Mutex<AllowBook>>,
    stop: Arc<AtomicBool>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    inflight: Arc<AtomicUsize>,
    idle: Duration,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut sock, _)) => {
                // Non-blocking. Do not set SO_RCVTIMEO or SO_SNDTIMEO.
                if sock.set_nonblocking(true).is_err() {
                    continue;
                }
                let _ = sock.set_nodelay(true);
                if !claim_conn_slot(&inflight) {
                    // Dropping a socket that still holds the request makes Linux send RST
                    // and the client never sees the 503.
                    let _ = discard_unread(&mut sock);
                    log_media_req(&sock, "unknown", "redacted", 503, 0, "none");
                    let _ = write_empty(&mut sock, 503);
                    let _ = sock.shutdown(Shutdown::Write);
                    continue;
                }
                let book_c = Arc::clone(&book);
                let stop_c = Arc::clone(&stop);
                let inflight_c = Arc::clone(&inflight);
                let log_dir = cast_log_override_dir();
                let spawned = thread::Builder::new()
                    .name("cast-http".into())
                    .spawn(move || {
                        apply_cast_log_dir(log_dir);
                        struct Slot(Arc<AtomicUsize>);
                        impl Drop for Slot {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        let _slot = Slot(inflight_c);
                        let _ = serve_client(&mut sock, &book_c, &stop_c, idle);
                    });
                match spawned {
                    Ok(handle) => {
                        if let Ok(mut list) = workers.lock() {
                            list.retain(|h| !h.is_finished());
                            list.push(handle);
                        }
                    }
                    Err(_) => {
                        inflight.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // Dropping the listener here closes the port before `shutdown` returns.
    drop(listener);
}

fn claim_conn_slot(inflight: &AtomicUsize) -> bool {
    loop {
        let current = inflight.load(Ordering::SeqCst);
        if current >= HTTP_CONN_CAP {
            return false;
        }
        if inflight
            .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return true;
        }
    }
}

struct MediaReqLog {
    from: String,
    method: String,
    path: String,
    status: u16,
    bytes: u64,
    ctype: String,
}

impl MediaReqLog {
    fn begin(sock: &TcpStream, method: &str, url: &str) -> Self {
        Self {
            from: peer_ipv4(sock),
            method: http_method_token(method),
            path: masked_media_path(url),
            status: 0,
            bytes: 0,
            ctype: "none".into(),
        }
    }
}

impl Drop for MediaReqLog {
    fn drop(&mut self) {
        log_media_req_fields(
            &self.from,
            &self.method,
            &self.path,
            self.status,
            self.bytes,
            &self.ctype,
        );
    }
}

fn peer_ipv4(sock: &TcpStream) -> String {
    match sock.peer_addr() {
        Ok(addr) => match addr.ip() {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(_) => "unknown".into(),
        },
        Err(_) => "unknown".into(),
    }
}

fn http_method_token(method: &str) -> String {
    if !method.is_empty()
        && method.len() <= 16
        && method.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        method.to_string()
    } else {
        "unknown".into()
    }
}

/// `/m/<token>/<key>` becomes `/m/<key>?t=***`. The token is never copied into the log.
fn masked_media_path(url_or_path: &str) -> String {
    let bare = url_or_path.split(['?', '#']).next().unwrap_or(url_or_path);
    let path = if let Some(rest) = bare
        .strip_prefix("http://")
        .or_else(|| bare.strip_prefix("https://"))
    {
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => return "redacted".into(),
        }
    } else {
        bare
    };
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    if parts.next() != Some("m") {
        return "redacted".into();
    }
    let Some(token) = parts.next() else {
        return "redacted".into();
    };
    let Some(key) = parts.next() else {
        return "redacted".into();
    };
    if parts.next().is_some() || token.is_empty() || !log_key_ok(key) {
        return "redacted".into();
    }
    format!("/m/{key}?t=***")
}

fn log_key_ok(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 80
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn log_ctype(ctype: &str) -> String {
    match ctype {
        "image/jpeg" | "image/png" | "video/mp4" => ctype.to_string(),
        _ => "none".into(),
    }
}

fn mask_load_url(url: &str) -> String {
    let host = media_url_host(url).unwrap_or_else(|| "host-unknown".into());
    let path = masked_media_path(url);
    if path == "redacted" {
        "redacted".into()
    } else {
        format!("http://{host}{path}")
    }
}

fn log_media_req(sock: &TcpStream, method: &str, path: &str, status: u16, bytes: u64, ctype: &str) {
    log_media_req_fields(
        &peer_ipv4(sock),
        &http_method_token(method),
        path,
        status,
        bytes,
        ctype,
    );
}

fn log_media_req_fields(
    from: &str,
    method: &str,
    path: &str,
    status: u16,
    bytes: u64,
    ctype: &str,
) {
    cast_log(
        "media_req",
        &format!(
            "from={from} method={method} path={path} status={status} bytes={bytes} ctype={ctype}"
        ),
    );
}

fn serve_client(
    sock: &mut TcpStream,
    book: &Mutex<AllowBook>,
    stop: &AtomicBool,
    idle: Duration,
) -> std::io::Result<()> {
    let mut pace = IoPace::new(idle);
    let raw = read_headers(sock, stop, &mut pace)?;
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let url = parts.next().unwrap_or("/").to_string();
    let mut rec = MediaReqLog::begin(sock, method, &url);
    if method != "GET" && method != "HEAD" {
        rec.status = 405;
        return write_empty(sock, 405);
    }
    let mut range = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_string());
            }
        }
    }
    let (token, key) = media_token_key(&url).unwrap_or_else(|| (String::new(), String::new()));
    let entry = {
        let guard = match book.lock() {
            Ok(g) => g,
            Err(_) => {
                rec.status = 404;
                return write_empty(sock, 404);
            }
        };
        match resolve_media_url(&url, &guard) {
            Ok(e) => e.clone(),
            Err(_) => {
                rec.status = 404;
                return write_empty(sock, 404);
            }
        }
    };
    rec.ctype = log_ctype(&entry.content_type);
    let prepared = match transform_media(&entry) {
        Ok(body) => body,
        Err(_) => {
            rec.status = 404;
            return write_empty(sock, 404);
        }
    };
    let len = prepared.len;
    let ctype = prepared.content_type;
    let file = prepared.file;
    let head = method == "HEAD";
    let result = if let Some(spec) = range {
        match parse_range(&spec, len) {
            Some((start, end)) => {
                let take = end - start + 1;
                rec.status = if start == 0 && take == len { 200 } else { 206 };
                let wrote = write_file(
                    sock, file, &ctype, start, end, len, head, stop, book, &token, &key, &mut pace,
                );
                if wrote.is_ok() && !head {
                    rec.bytes = take;
                }
                wrote
            }
            None => {
                rec.status = 416;
                write_empty(sock, 416)
            }
        }
    } else if len == 0 {
        rec.status = 200;
        write_file(
            sock, file, &ctype, 0, 0, 0, head, stop, book, &token, &key, &mut pace,
        )
    } else {
        rec.status = 200;
        let wrote = write_file(
            sock,
            file,
            &ctype,
            0,
            len - 1,
            len,
            head,
            stop,
            book,
            &token,
            &key,
            &mut pace,
        );
        if wrote.is_ok() && !head {
            rec.bytes = len;
        }
        wrote
    };
    result
}

struct IoPace {
    last: Instant,
    idle: Duration,
}

impl IoPace {
    fn new(idle: Duration) -> Self {
        Self {
            last: Instant::now(),
            idle,
        }
    }

    fn advance(&mut self) {
        self.last = Instant::now();
    }

    fn stalled(&self) -> bool {
        self.last.elapsed() >= self.idle
    }
}

fn discard_unread(sock: &mut TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn close_stalled(sock: &mut TcpStream) -> std::io::Error {
    // Write shutdown sends FIN so the TV's read returns. Drop closes the fd
    // when the worker returns, which frees the connection slot.
    let _ = discard_unread(sock);
    let _ = sock.shutdown(Shutdown::Write);
    std::io::Error::new(ErrorKind::TimedOut, "no forward progress")
}

fn io_wait(sock: &mut TcpStream, pace: &IoPace, stop: &AtomicBool) -> std::io::Result<()> {
    if stop.load(Ordering::SeqCst) {
        let _ = sock.shutdown(Shutdown::Both);
        return Err(std::io::Error::new(
            ErrorKind::ConnectionAborted,
            "cast stopped",
        ));
    }
    if pace.stalled() {
        return Err(close_stalled(sock));
    }
    let remain = pace.idle.saturating_sub(pace.last.elapsed());
    thread::sleep(HTTP_POLL.min(remain));
    if stop.load(Ordering::SeqCst) {
        let _ = sock.shutdown(Shutdown::Both);
        return Err(std::io::Error::new(
            ErrorKind::ConnectionAborted,
            "cast stopped",
        ));
    }
    if pace.stalled() {
        return Err(close_stalled(sock));
    }
    Ok(())
}

fn media_token_key(url: &str) -> Option<(String, String)> {
    let bare = url.split(['?', '#']).next().unwrap_or(url);
    let mut parts = bare.split('/').filter(|s| !s.is_empty());
    if parts.next()? != "m" {
        return None;
    }
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

fn grant_still_valid(book: &Mutex<AllowBook>, token: &str, key: &str) -> bool {
    let Ok(guard) = book.lock() else {
        return false;
    };
    token.len() == guard.token.len()
        && ct_eq(token.as_bytes(), guard.token.as_bytes())
        && guard.files.contains_key(key)
}

fn transfer_aborted(stop: &AtomicBool, book: &Mutex<AllowBook>, token: &str, key: &str) -> bool {
    stop.load(Ordering::SeqCst) || !grant_still_valid(book, token, key)
}

/// Bytes to send after the playlist token check.
///
/// This is the identity transform: the allowlisted file itself. A later slideshow
/// crop sidecar can replace a still here (new bytes and content type) without
/// changing the token check, the path check, or the socket loop.
struct PreparedMedia {
    file: File,
    len: u64,
    content_type: String,
}

fn transform_media(entry: &AllowEntry) -> std::io::Result<PreparedMedia> {
    let file = File::open(&entry.path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a file",
        ));
    }
    Ok(PreparedMedia {
        len: meta.len(),
        content_type: entry.content_type.clone(),
        file,
    })
}

fn read_headers(
    sock: &mut TcpStream,
    stop: &AtomicBool,
    pace: &mut IoPace,
) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 512];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
        match sock.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                pace.advance();
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => io_wait(sock, pace, stop)?,
            Err(e) => return Err(e),
        }
    }
    Ok(buf)
}

fn write_empty(sock: &mut TcpStream, code: u16) -> std::io::Result<()> {
    let reason = match code {
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let msg = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let mut data = msg.as_bytes();
    let deadline = Instant::now() + Duration::from_millis(500);
    while !data.is_empty() {
        match sock.write(data) {
            Ok(0) => return Err(std::io::Error::new(ErrorKind::WriteZero, "closed")),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(HTTP_POLL);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_all_checked(
    sock: &mut TcpStream,
    mut data: &[u8],
    stop: &AtomicBool,
    book: &Mutex<AllowBook>,
    token: &str,
    key: &str,
    pace: &mut IoPace,
) -> std::io::Result<()> {
    while !data.is_empty() {
        if transfer_aborted(stop, book, token, key) {
            let _ = sock.shutdown(Shutdown::Both);
            return Err(std::io::Error::new(
                ErrorKind::ConnectionAborted,
                "cast stopped",
            ));
        }
        match sock.write(data) {
            Ok(0) => return Err(std::io::Error::new(ErrorKind::WriteZero, "closed")),
            Ok(n) => {
                // Only these bytes were accepted. A later WouldBlock retries the rest, never these.
                pace.advance();
                data = &data[n..];
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => io_wait(sock, pace, stop)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_file(
    sock: &mut TcpStream,
    mut file: File,
    ctype: &str,
    start: u64,
    end_inclusive: u64,
    total: u64,
    head: bool,
    stop: &AtomicBool,
    book: &Mutex<AllowBook>,
    token: &str,
    key: &str,
    pace: &mut IoPace,
) -> std::io::Result<()> {
    if transfer_aborted(stop, book, token, key) {
        let _ = sock.shutdown(Shutdown::Both);
        return Err(std::io::Error::new(
            ErrorKind::ConnectionAborted,
            "cast stopped",
        ));
    }
    if total == 0 {
        let msg = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: 0\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
        );
        return write_all_checked(sock, msg.as_bytes(), stop, book, token, key, pace);
    }
    if start > end_inclusive || end_inclusive >= total {
        return write_empty(sock, 416);
    }
    let take = end_inclusive - start + 1;
    let partial = start != 0 || take != total;
    let status = if partial { 206 } else { 200 };
    let reason = if partial { "Partial Content" } else { "OK" };
    let mut msg = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {take}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n"
    );
    if partial {
        msg.push_str(&format!(
            "Content-Range: bytes {start}-{end_inclusive}/{total}\r\n"
        ));
    }
    msg.push_str("\r\n");
    write_all_checked(sock, msg.as_bytes(), stop, book, token, key, pace)?;
    if head {
        return Ok(());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut left = take;
    let mut buf = vec![0u8; HTTP_CHUNK];
    while left > 0 {
        if transfer_aborted(stop, book, token, key) {
            let _ = sock.shutdown(Shutdown::Both);
            return Err(std::io::Error::new(
                ErrorKind::ConnectionAborted,
                "cast stopped",
            ));
        }
        let chunk = left.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..chunk])?;
        if n == 0 {
            break;
        }
        write_all_checked(sock, &buf[..n], stop, book, token, key, pace)?;
        left -= n as u64;
    }
    Ok(())
}

/// `bytes=start-end`, `bytes=start-`, or `bytes=-suffix`. End is inclusive.
pub fn parse_range(spec: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let spec = spec.trim();
    let rest = spec
        .strip_prefix("bytes=")
        .or_else(|| spec.strip_prefix("bytes: "))?;
    let range = rest.split(',').next()?.trim();
    let (start_s, end_s) = range.split_once('-')?;
    if start_s.is_empty() {
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = size.saturating_sub(suffix);
        return Some((start, size - 1));
    }
    let start: u64 = start_s.parse().ok()?;
    if start >= size {
        return None;
    }
    let end = if end_s.is_empty() {
        size - 1
    } else {
        let end: u64 = end_s.parse().ok()?;
        end.min(size - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Largest Cast frame we will assemble. A hostile length must not pin memory.
const CAST_FRAME_MAX: usize = 1_048_576;
const READ_POLL: Duration = Duration::from_millis(15);

/// `TimedOut` (Windows 10060) and `WouldBlock` both mean the socket has no
/// bytes ready. rustls treats only `WouldBlock` as idle, and a Windows
/// `SO_RCVTIMEO` leaves the socket undefined, so the cast socket never uses one.
/// Raw 110 is `ERROR_OPEN_FAILED` on Windows, not a timeout, and a message that
/// merely contains the word "timeout" is not idle either.
fn io_is_idle(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
        || matches!(err.raw_os_error(), Some(10060) | Some(10035) | Some(11))
}

/// Retry an idle write so a stall after the first bytes of a frame does not
/// fail `write_all` and drop the rest. Returns once at least one byte is in,
/// or `WouldBlock` at `deadline`.
fn write_some_retry_idle(
    write: &mut dyn Write,
    buf: &[u8],
    deadline: Instant,
) -> std::io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    loop {
        match write.write(buf) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => return Ok(n),
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if io_is_idle(&err) => {
                if Instant::now() >= deadline {
                    return Err(ErrorKind::WouldBlock.into());
                }
                thread::sleep(READ_POLL.min(deadline.saturating_duration_since(Instant::now())));
            }
            Err(err) => return Err(err),
        }
    }
}

struct IdleSocket {
    inner: TcpStream,
}

impl Read for IdleSocket {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.inner.read(buf) {
            Err(err) if io_is_idle(&err) => Err(ErrorKind::WouldBlock.into()),
            other => other,
        }
    }
}

impl Write for IdleSocket {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.inner.write(buf) {
            Err(err) if io_is_idle(&err) => Err(ErrorKind::WouldBlock.into()),
            other => other,
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.inner.flush() {
            Err(err) if io_is_idle(&err) => Err(ErrorKind::WouldBlock.into()),
            other => other,
        }
    }
}

/// Plaintext framing in front of rustls.
///
/// `MessageManager` uses `read_exact`. A short read followed by `TimedOut`
/// drops the bytes already taken and desynchronizes the stream. This reader
/// keeps every byte it has seen, releases a length prefix only after the whole
/// frame is buffered, and turns either idle error into "no data yet".
struct CastPlain<C, S> {
    inner: StreamOwned<C, IdleSocket>,
    pending: Vec<u8>,
    /// Bytes of the current frame not yet handed to `read_exact`, including the
    /// 4-byte length. Zero means the next read must start a new frame.
    frame_left: usize,
    pace: Arc<Mutex<Duration>>,
    _side: PhantomData<S>,
}

impl<C, S> CastPlain<C, S>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
    StreamOwned<C, IdleSocket>: Read + Write,
{
    fn new(inner: StreamOwned<C, IdleSocket>, pace: Arc<Mutex<Duration>>) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            frame_left: 0,
            pace,
            _side: PhantomData,
        }
    }

    fn pace(&self) -> Duration {
        self.pace.lock().map(|g| *g).unwrap_or(READ_SLICE)
    }

    fn have_full_frame(&self) -> bool {
        if self.pending.len() < 4 {
            return false;
        }
        let len = u32::from_be_bytes(self.pending[..4].try_into().unwrap()) as usize;
        len <= CAST_FRAME_MAX && self.pending.len() >= 4 + len
    }

    /// Copy already-decrypted TLS plaintext into `pending` before any further
    /// socket read. `Stream::read` can return `TimedOut`/`WouldBlock` from
    /// `complete_io` even after rustls has plaintext waiting.
    fn drain_plain(&mut self) -> std::io::Result<()> {
        let mut tmp = [0u8; 8192];
        loop {
            let n = match self.inner.conn.reader().read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if io_is_idle(&err) => break,
                Err(err) => return Err(err),
            };
            self.pending.extend_from_slice(&tmp[..n]);
        }
        Ok(())
    }

    fn pull_some(&mut self) -> std::io::Result<bool> {
        let before = self.pending.len();
        self.drain_plain()?;
        if self.have_full_frame() {
            return Ok(true);
        }
        let mut tmp = [0u8; 8192];
        match self.inner.read(&mut tmp) {
            Ok(0) => Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                self.pending.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(err) if io_is_idle(&err) => {
                self.drain_plain()?;
                Ok(self.pending.len() > before || self.have_full_frame())
            }
            Err(err) => Err(err),
        }
    }

    fn buffer_full_frame(&mut self) -> std::io::Result<()> {
        let idle_deadline = Instant::now() + self.pace();
        let mut started_frame = Instant::now();
        let mut saw_byte = !self.pending.is_empty();
        loop {
            if self.pending.len() >= 4 {
                let len = u32::from_be_bytes(self.pending[..4].try_into().unwrap()) as usize;
                if len > CAST_FRAME_MAX {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "cast frame is too large",
                    ));
                }
            }
            if self.have_full_frame() {
                return Ok(());
            }
            let got = self.pull_some()?;
            if got {
                if !saw_byte && !self.pending.is_empty() {
                    saw_byte = true;
                    started_frame = Instant::now();
                }
                continue;
            }
            let now = Instant::now();
            // A frame that has already started must be finished. Returning idle
            // here would drop the length prefix on the next `read_exact`.
            let deadline = if saw_byte {
                idle_deadline.max(started_frame + LINK_DEAD_AFTER)
            } else {
                idle_deadline
            };
            if now >= deadline {
                return Err(ErrorKind::WouldBlock.into());
            }
            thread::sleep(READ_POLL.min(deadline.saturating_duration_since(now)));
        }
    }
}

impl<C, S> Read for CastPlain<C, S>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
    StreamOwned<C, IdleSocket>: Read + Write,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.frame_left == 0 {
            self.buffer_full_frame()?;
            let len = u32::from_be_bytes(self.pending[..4].try_into().unwrap()) as usize;
            self.frame_left = 4 + len;
        }
        let n = buf.len().min(self.frame_left);
        if self.pending.len() < n {
            return Err(ErrorKind::WouldBlock.into());
        }
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        self.frame_left -= n;
        Ok(n)
    }
}

impl<C, S> Write for CastPlain<C, S>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<S>>,
    S: rustls::SideData,
    StreamOwned<C, IdleSocket>: Read + Write,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let deadline = Instant::now() + self.pace().max(READ_SLICE);
        write_some_retry_idle(&mut self.inner, buf, deadline)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

type CastStream = CastPlain<ClientConnection, rustls::client::ClientConnectionData>;

struct CastLink {
    tcp: TcpStream,
    pace: Arc<Mutex<Duration>>,
    mm: Rc<MessageManager<CastStream>>,
    heartbeat: HeartbeatChannel<'static, CastStream>,
    connection: ConnectionChannel<'static, CastStream>,
    receiver: ReceiverChannel<'static, CastStream>,
    media: MediaChannel<'static, CastStream>,
    transport_id: String,
    session_id: String,
    /// The first outbound PING and the first PONG are logged. Later heartbeats are quiet.
    logged_ping: Cell<bool>,
    logged_pong: Cell<bool>,
    /// Set by Disconnect so a slideshow load still in the queue is not sent.
    cancel: Arc<AtomicBool>,
    /// Shared with the caller so a dead link still produces exactly one line.
    disconnect_logged: Arc<AtomicBool>,
    /// Duration the page timer used for this LOAD. Read only for `source=timer`.
    photo_timer_ms: Cell<Option<u64>>,
}

impl CastLink {
    /// TCP + TLS only. Does not send the virtual CONNECT. Tests use this to prove
    /// the receiver ignores everything that arrives before CONNECT.
    fn open_tls(host: Ipv4Addr, port: u16) -> Result<Self, String> {
        cast_log("connect", &format!("{host}:{port}"));
        let addr = SocketAddr::from((host, port));
        let mut tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(4)).map_err(|_| {
            let reason = format!(
                "TV unreachable at {host}. It may be off, or this PC and the TV are not on the same Wi-Fi."
            );
            log_connect_error(&reason, host);
            reason
        })?;
        tcp.set_nodelay(true).ok();
        // No SO_RCVTIMEO. On Windows a read timeout is TimedOut (10060), rustls
        // treats only WouldBlock as idle, and the socket is left undefined.
        // Non-blocking has to be set on this handle: a later clone does not
        // inherit it. The handshake retries WouldBlock until its own deadline.
        tcp.set_read_timeout(None).ok();
        tcp.set_write_timeout(None).ok();
        tcp.set_nonblocking(true)
            .map_err(|e| connect_fail(format!("cast socket: {e}")))?;
        let tcp_ctl = tcp
            .try_clone()
            .map_err(|e| connect_fail(format!("cast socket: {e}")))?;

        let _ = default_provider().install_default();
        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth();
        let name = ServerName::try_from(host.to_string().as_str())
            .map_err(|_| connect_fail("TV address is not a valid cast host.".to_string()))?
            .to_owned();
        let mut conn = ClientConnection::new(Arc::new(config), name)
            .map_err(|e| connect_fail(format!("cast tls: {e}")))?;
        drive_tls(&mut conn, &mut tcp).map_err(connect_fail)?;
        let pace = Arc::new(Mutex::new(READ_SLICE));
        let stream = CastPlain::new(
            StreamOwned::new(conn, IdleSocket { inner: tcp }),
            Arc::clone(&pace),
        );
        let mm = Rc::new(MessageManager::new(stream));
        let heartbeat = HeartbeatChannel::new("sender-0", "receiver-0", Rc::clone(&mm));
        let connection = ConnectionChannel::new("sender-0", Rc::clone(&mm));
        let receiver = ReceiverChannel::new("sender-0", "receiver-0", Rc::clone(&mm));
        let media = MediaChannel::new("sender-0", Rc::clone(&mm));
        cast_log("tls", &format!("{host}:{port}"));
        Ok(Self {
            tcp: tcp_ctl,
            pace,
            mm,
            heartbeat,
            connection,
            receiver,
            media,
            transport_id: String::new(),
            session_id: String::new(),
            logged_ping: Cell::new(false),
            logged_pong: Cell::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            disconnect_logged: Arc::new(AtomicBool::new(false)),
            photo_timer_ms: Cell::new(None),
        })
    }

    fn use_cancel(&mut self, cancel: Arc<AtomicBool>) {
        self.cancel = cancel;
    }

    fn use_disconnect_logged(&mut self, flag: Arc<AtomicBool>) {
        self.disconnect_logged = flag;
    }

    fn log_disconnect(&self, detail: &str) {
        log_disconnect_once(&self.disconnect_logged, detail);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn connect(host: Ipv4Addr, port: u16) -> Result<Self, String> {
        let link = Self::open_tls(host, port)?;
        link.connection
            .connect("receiver-0")
            .map_err(|e| connect_fail(format!("Could not connect to the TV ({e}).")))?;
        cast_log("receiver-connect", "receiver-0");
        // Handshake is done. Further reads must not use SO_RCVTIMEO.
        link.arm_idle();
        link.ping().map_err(connect_fail)?;
        Ok(link)
    }

    fn set_pace(&self, dur: Duration) {
        if let Ok(mut pace) = self.pace.lock() {
            *pace = dur;
        }
    }

    fn arm_idle(&self) {
        // The read handle was set in `open_tls`. Repeat on the clone so a
        // shared socket option cannot put SO_RCVTIMEO back. Non-blocking mode
        // itself is per handle on Windows and is not inherited by this clone.
        self.tcp.set_read_timeout(None).ok();
        self.tcp.set_write_timeout(None).ok();
        self.tcp.set_nonblocking(true).ok();
    }

    fn set_read_timeout(&self, dur: Duration) {
        self.set_pace(dur);
        self.arm_idle();
    }

    fn send_json(
        &self,
        namespace: &str,
        destination: &str,
        payload: &str,
    ) -> Result<(), rust_cast::errors::Error> {
        self.mm.send(CastMessage {
            namespace: namespace.to_string(),
            source: "sender-0".to_string(),
            destination: destination.to_string(),
            payload: CastMessagePayload::String(payload.to_string()),
        })
    }

    fn launch_receiver(&mut self) -> Result<(), String> {
        let request_id = self.mm.generate_request_id().get();
        self.send_json(NS_RECEIVER, "receiver-0", &launch_payload(request_id))
            .map_err(|e| connect_fail(map_launch_failure(&e.to_string())))?;
        cast_log("launch", &format!("requestId={request_id} appId=CC1AD845"));
        let app = self.wait_launch(request_id).map_err(connect_fail)?;
        self.connection
            .connect(app.transport_id.as_str())
            .map_err(|e| connect_fail(format!("Could not connect to the TV player ({e}).")))?;
        cast_log("transport-connect", &app.transport_id);
        self.transport_id = app.transport_id;
        self.session_id = app.session_id;
        Ok(())
    }

    fn log_load(
        &self,
        host: &str,
        request_id: u32,
        origin: CastOrigin,
        item: u32,
        media_type: &str,
        url: &str,
        content_type: &str,
        tail: &str,
    ) {
        let detail = load_log_detail(
            host,
            request_id,
            origin.as_str(),
            item,
            media_type,
            url,
            content_type,
            tail,
        );
        let session_ms = if origin == CastOrigin::Timer {
            self.photo_timer_ms.get()
        } else {
            None
        };
        cast_log("load", &with_timer_dur(detail, origin, session_ms));
    }

    fn load_url(
        &self,
        url: &str,
        content_type: &str,
        autoplay: bool,
        origin: CastOrigin,
        item: u32,
        media_type: &str,
    ) -> Result<i32, String> {
        if self.cancelled() {
            return Err(CAST_CANCELLED.to_string());
        }
        let request_id = self.mm.generate_request_id().get();
        let payload = load_payload(request_id, &self.session_id, url, content_type, autoplay);
        let host = media_url_host(url).unwrap_or_else(|| "host-unknown".into());
        let media_type = media_type_label(media_type);
        if let Err(e) = self.send_json(NS_MEDIA, &self.transport_id, &payload) {
            let err = map_load_error(&e.to_string());
            self.log_load(
                &host,
                request_id,
                origin,
                item,
                media_type,
                url,
                content_type,
                &format!("error={err}"),
            );
            return Err(err);
        }
        match self.wait_load(request_id) {
            Ok(reply) => {
                let sid = reply.session_id;
                let tail = reply.log_tail();
                self.log_load(
                    &host,
                    request_id,
                    origin,
                    item,
                    media_type,
                    url,
                    content_type,
                    &tail,
                );
                Ok(sid)
            }
            Err(fail) if fail.user == CAST_CANCELLED => Err(fail.user),
            Err(fail) => {
                self.log_load(
                    &host,
                    request_id,
                    origin,
                    item,
                    media_type,
                    url,
                    content_type,
                    &fail.log_tail,
                );
                Err(fail.user)
            }
        }
    }

    /// CONNECT receiver-0, then LAUNCH, then wait for that requestId.
    fn wait_launch(&self, request_id: u32) -> Result<Application, String> {
        self.wait_matching(
            LAUNCH_DEADLINE,
            WaitMsg::Text(LAUNCH_TIMEOUT_MESSAGE),
            WaitExpect::Launch(request_id),
        )
        .and_then(|hit| match hit {
            Waited::Launched(app) => Ok(app),
            Waited::Loaded(_) | Waited::LoadRejected { .. } | Waited::Media(_) => {
                Err(LAUNCH_TIMEOUT_MESSAGE.to_string())
            }
        })
    }

    fn wait_load(&self, request_id: u32) -> Result<LoadReply, LoadWaitError> {
        // A LOAD_FAILED returns as soon as the TV says so. The timeout sentence
        // is only built after the wait gives up.
        match self.wait_matching(
            LOAD_DEADLINE,
            WaitMsg::LoadNotice,
            WaitExpect::Load(request_id),
        ) {
            Ok(Waited::Loaded(reply)) => Ok(reply),
            Ok(Waited::LoadRejected { kind }) => Err(LoadWaitError {
                user: load_reject_user(kind),
                log_tail: format!("reply={kind} playerState=none idleReason=none"),
            }),
            Ok(Waited::Launched(_) | Waited::Media(_)) => {
                let user = media_load_notice();
                Err(LoadWaitError {
                    log_tail: format!("error={user}"),
                    user,
                })
            }
            Err(err) => Err(LoadWaitError {
                log_tail: format!("error={err}"),
                user: err,
            }),
        }
    }

    fn wait_matching(
        &self,
        deadline: Duration,
        timeout_message: WaitMsg<'_>,
        expect: WaitExpect,
    ) -> Result<Waited, String> {
        let started = Instant::now();
        let mut last_heard = Instant::now();
        let mut last_ping = Instant::now();
        self.arm_idle();
        loop {
            if self.cancelled() {
                return Err(CAST_CANCELLED.to_string());
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            let silence = LINK_DEAD_AFTER.saturating_sub(last_heard.elapsed());
            // A zero slice still drains plaintext rustls already decrypted.
            let slice = remaining.min(silence).min(READ_SLICE);
            self.set_pace(slice);
            let msg = match self.mm.receive() {
                Ok(msg) => msg,
                Err(e) if cast_err_is_timeout(&e) => {
                    if slice.is_zero() {
                        return Err(self.note_timeout(&timeout_message));
                    }
                    if last_ping.elapsed() >= PING_EVERY {
                        last_ping = Instant::now();
                        let _ = self.ping();
                    }
                    continue;
                }
                Err(e) => {
                    return Err(if is_cast_timeout(&e.to_string()) {
                        self.note_timeout(&timeout_message)
                    } else {
                        link_down_message()
                    });
                }
            };
            last_heard = Instant::now();
            if self.answer_heartbeat(&msg) {
                continue;
            }
            match expect {
                WaitExpect::Launch(id) => {
                    if !self.receiver.can_handle(&msg) {
                        continue;
                    }
                    match self.receiver.parse(&msg) {
                        Ok(ReceiverResponse::Status(status)) if status.request_id == id => {
                            let app = status.applications.into_iter().next().ok_or_else(|| {
                                "The TV opened a player with no session.".to_string()
                            })?;
                            return Ok(Waited::Launched(app));
                        }
                        Ok(ReceiverResponse::LaunchError(err)) if err.request_id == id => {
                            let reason = err
                                .reason
                                .filter(|r| !r.trim().is_empty())
                                .unwrap_or_else(|| "unknown".into());
                            return Err(format!("The TV refused to open the player ({reason})."));
                        }
                        Ok(ReceiverResponse::InvalidRequest(err)) if err.request_id == id => {
                            let reason = err.reason.unwrap_or_else(|| "invalid request".into());
                            return Err(format!("The TV refused to open the player ({reason})."));
                        }
                        _ => continue,
                    }
                }
                WaitExpect::Load(id) => {
                    if !self.media.can_handle(&msg) {
                        continue;
                    }
                    match self.media.parse(&msg) {
                        Ok(MediaResponse::Status(status)) if status.request_id == id => {
                            let Some(entry) = status.entries.first() else {
                                return Ok(Waited::LoadRejected { kind: "MEDIA_STATUS" });
                            };
                            return Ok(Waited::Loaded(LoadReply {
                                session_id: entry.media_session_id,
                                player_state: entry.player_state.to_string(),
                                idle_reason: idle_reason_token(entry.idle_reason).to_string(),
                            }));
                        }
                        Ok(MediaResponse::LoadFailed(err)) if err.request_id == id => {
                            let _ = err;
                            return Ok(Waited::LoadRejected { kind: "LOAD_FAILED" });
                        }
                        Ok(MediaResponse::LoadCancelled(err)) if err.request_id == id => {
                            let _ = err;
                            return Ok(Waited::LoadRejected {
                                kind: "LOAD_CANCELLED",
                            });
                        }
                        Ok(MediaResponse::InvalidRequest(err)) if err.request_id == id => {
                            let _ = err;
                            return Ok(Waited::LoadRejected {
                                kind: "INVALID_REQUEST",
                            });
                        }
                        Ok(MediaResponse::InvalidPlayerState(err)) if err.request_id == id => {
                            let _ = err;
                            return Ok(Waited::LoadRejected {
                                kind: "INVALID_PLAYER_STATE",
                            });
                        }
                        Ok(MediaResponse::Error(_)) if parsed_error_matches(&msg, id, true) => {
                            return Ok(Waited::LoadRejected { kind: "ERROR" });
                        }
                        _ => {
                            if let Some(kind) = media_error_reply(&msg, id) {
                                return Ok(Waited::LoadRejected { kind });
                            }
                            continue;
                        }
                    }
                }
                WaitExpect::Media(id) => {
                    if !self.media.can_handle(&msg) {
                        continue;
                    }
                    match self.media.parse(&msg) {
                        Ok(MediaResponse::Status(status)) if status.request_id == id => {
                            let state = status.entries.first().map(|e| e.player_state).ok_or_else(
                                || "The TV answered without a player state.".to_string(),
                            )?;
                            return Ok(Waited::Media(state));
                        }
                        Ok(MediaResponse::InvalidRequest(err)) if err.request_id == id => {
                            let reason = err.reason.unwrap_or_else(|| "invalid request".into());
                            return Err(format!("The TV refused the media command ({reason})."));
                        }
                        Ok(MediaResponse::InvalidPlayerState(err)) if err.request_id == id => {
                            let _ = err;
                            return Err(
                                "The TV refused the media command (invalid player state).".into()
                            );
                        }
                        Ok(MediaResponse::LoadFailed(err)) if err.request_id == id => {
                            let _ = err;
                            return Err("The TV refused the media command (LOAD_FAILED).".into());
                        }
                        Ok(MediaResponse::Error(_)) if parsed_error_matches(&msg, id, false) => {
                            return Err("The TV refused the media command (ERROR).".into());
                        }
                        _ => {
                            if let Some(kind) = media_error_reply(&msg, id) {
                                return Err(format!(
                                    "The TV refused the media command ({kind})."
                                ));
                            }
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// PAUSE, PLAY, STOP, and SEEK go to the transport id with this mediaSessionId.
    /// One log line carries the requestId and the TV's reply or error.
    fn media_cmd(
        &self,
        typ: &str,
        sid: i32,
        current_time: Option<f32>,
        deadline: Duration,
        origin: CastOrigin,
    ) -> Result<PlayerState, String> {
        let request_id = self.mm.generate_request_id().get();
        let payload = match current_time {
            Some(secs) => serde_json::json!({
                "requestId": request_id,
                "mediaSessionId": sid,
                "type": typ,
                "currentTime": secs,
            }),
            None => serde_json::json!({
                "requestId": request_id,
                "mediaSessionId": sid,
                "type": typ,
            }),
        };
        let step = typ.to_ascii_lowercase();
        let ids = format!(
            "requestId={request_id} mediaSessionId={sid} source={}",
            origin.as_str()
        );
        if let Err(e) = self.send_json(NS_MEDIA, &self.transport_id, &payload.to_string()) {
            let err = format!("The TV did not take {typ} ({e}).");
            cast_log(&step, &format!("{ids} error={err}"));
            return Err(err);
        }
        match self.wait_matching(
            deadline,
            WaitMsg::Text(MEDIA_FETCH_MESSAGE),
            WaitExpect::Media(request_id),
        ) {
            Ok(Waited::Media(state)) => {
                cast_log(&step, &format!("{ids} reply={state}"));
                Ok(state)
            }
            Ok(_) => {
                let err = cast_panel_message(CastPanelCause::MediaCommandTimeout);
                cast_log(&step, &format!("{ids} error={err}"));
                Err(err)
            }
            Err(err)
                if matches!(typ, "PLAY" | "PAUSE" | "SEEK") && media_cmd_unanswered(&err) =>
            {
                // The TV often never replies to PLAY or PAUSE for a still, and a
                // missed SEEK must not become an error. A missed reply must not
                // become "TV unreachable".
                cast_log(&step, &format!("{ids} warning=unanswered"));
                Ok(if typ == "PAUSE" {
                    PlayerState::Paused
                } else {
                    PlayerState::Playing
                })
            }
            Err(err) => {
                cast_log(&step, &format!("{ids} error={err}"));
                Err(err)
            }
        }
    }

    fn pause(&self, sid: i32, origin: CastOrigin) -> Result<PlayerState, String> {
        self.media_cmd("PAUSE", sid, None, MEDIA_CMD_DEADLINE, origin)
    }

    fn play(&self, sid: i32, origin: CastOrigin) -> Result<PlayerState, String> {
        self.media_cmd("PLAY", sid, None, MEDIA_CMD_DEADLINE, origin)
    }

    fn seek(&self, sid: i32, seconds: f32, origin: CastOrigin) -> Result<PlayerState, String> {
        self.media_cmd("SEEK", sid, Some(seconds), MEDIA_CMD_DEADLINE, origin)
    }

    fn stop(&self, sid: i32, origin: CastOrigin) -> Result<PlayerState, String> {
        self.media_cmd("STOP", sid, None, Duration::from_millis(800), origin)
    }

    /// Media STOP, receiver STOP for this session, then CLOSE the transport.
    /// Always one `disconnect` log line, including when the link is already dead.
    fn finish_disconnect(&self, media_sid: Option<i32>, origin: CastOrigin) {
        let mut parts = vec![format!("source={}", origin.as_str())];
        if let Some(sid) = media_sid {
            if !self.transport_id.is_empty() {
                let request_id = self.mm.generate_request_id().get();
                let payload = serde_json::json!({
                    "requestId": request_id,
                    "mediaSessionId": sid,
                    "type": "STOP",
                });
                match self.send_json(NS_MEDIA, &self.transport_id, &payload.to_string()) {
                    Ok(()) => parts.push(format!("mediaStop={sid}")),
                    Err(_) => parts.push("mediaStop=error".into()),
                }
            }
        }
        if !self.session_id.is_empty() {
            let request_id = self.mm.generate_request_id().get();
            let payload = serde_json::json!({
                "type": "STOP",
                "requestId": request_id,
                "sessionId": self.session_id,
            });
            match self.send_json(NS_RECEIVER, "receiver-0", &payload.to_string()) {
                Ok(()) => parts.push(format!("receiverStop={}", self.session_id)),
                Err(_) => parts.push("receiverStop=error".into()),
            }
        }
        let dest = if self.transport_id.is_empty() {
            "receiver-0".to_string()
        } else {
            self.transport_id.clone()
        };
        match self.connection.disconnect(dest.as_str()) {
            Ok(()) => parts.push(format!("close={dest}")),
            Err(_) => parts.push("close=error".into()),
        }
        self.log_disconnect(&parts.join(" "));
    }

    /// One line for the first PING we send and the first PONG either way.
    fn answer_heartbeat(&self, msg: &CastMessage) -> bool {
        if !self.heartbeat.can_handle(msg) {
            return false;
        }
        match self.heartbeat.parse(msg) {
            Ok(HeartbeatResponse::Ping) => {
                let _ = self.heartbeat.pong();
                self.note_pong("sent");
            }
            Ok(HeartbeatResponse::Pong) => self.note_pong("received"),
            _ => {}
        }
        true
    }

    fn note_pong(&self, which: &str) {
        if !self.logged_pong.replace(true) {
            cast_log("pong", which);
        }
    }

    fn note_timeout(&self, timeout_message: &WaitMsg<'_>) -> String {
        let msg = timeout_message.resolve();
        cast_log("timeout", &msg);
        msg
    }

    /// `Ok(None)` is a quiet slice. `Ok(Some(_))` means the TV sent something,
    /// including a PONG, so the silence clock resets.
    fn pump(&self) -> Result<Option<PumpUpdate>, String> {
        self.set_read_timeout(READ_IDLE);
        match self.mm.receive() {
            Ok(msg) => Ok(Some(self.apply_message(&msg))),
            Err(e) if cast_err_is_timeout(&e) => Ok(None),
            Err(_) => Err(link_down_message()),
        }
    }

    fn apply_message(&self, msg: &CastMessage) -> PumpUpdate {
        if self.answer_heartbeat(msg) {
            return PumpUpdate::empty();
        }
        if self.media.can_handle(msg) {
            match self.media.parse(msg) {
                Ok(MediaResponse::Status(status)) => {
                    if let Some(entry) = status.entries.first() {
                        return PumpUpdate {
                            player: Some(entry.player_state),
                            media_session: Some(entry.media_session_id),
                            idle_reason: entry.idle_reason,
                        };
                    }
                }
                Ok(MediaResponse::LoadFailed(_)) => {
                    return PumpUpdate {
                        player: Some(PlayerState::Idle),
                        ..PumpUpdate::empty()
                    };
                }
                _ => {}
            }
        }
        PumpUpdate::empty()
    }

    fn ping(&self) -> Result<(), String> {
        match self.heartbeat.ping() {
            Ok(()) => {
                if !self.logged_ping.replace(true) {
                    cast_log("ping", "sent");
                }
                Ok(())
            }
            Err(err) if cast_err_is_timeout(&err) => Ok(()),
            Err(_) => Err(link_down_message()),
        }
    }
}

enum WaitExpect {
    Launch(u32),
    Load(u32),
    Media(u32),
}

/// Resolved only when a wait actually times out. `LoadNotice` shells out to
/// PowerShell on Windows, which must not run before the read loop.
enum WaitMsg<'a> {
    Text(&'a str),
    LoadNotice,
}

impl WaitMsg<'_> {
    fn resolve(&self) -> String {
        match self {
            WaitMsg::Text(text) => (*text).to_string(),
            WaitMsg::LoadNotice => media_load_notice(),
        }
    }
}

/// Finish TLS on a non-blocking socket. `complete_io` returns `WouldBlock`
/// (or Windows `TimedOut`) while the peer has not answered yet.
fn drive_tls(conn: &mut ClientConnection, tcp: &mut TcpStream) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(4);
    while conn.is_handshaking() {
        match conn.complete_io(tcp) {
            Ok((0, 0)) => {
                if Instant::now() >= deadline {
                    return Err("Could not open a secure connection to the TV.".into());
                }
                thread::sleep(READ_POLL.min(deadline.saturating_duration_since(Instant::now())));
            }
            Ok(_) => {}
            Err(err) if io_is_idle(&err) => {
                if Instant::now() >= deadline {
                    return Err("Could not open a secure connection to the TV.".into());
                }
                thread::sleep(READ_POLL.min(deadline.saturating_duration_since(Instant::now())));
            }
            Err(err) => {
                return Err(format!(
                    "Could not open a secure connection to the TV ({err})."
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct LoadReply {
    session_id: i32,
    player_state: String,
    idle_reason: String,
}

impl LoadReply {
    fn log_tail(&self) -> String {
        format!(
            "reply=MEDIA_STATUS playerState={} idleReason={} mediaSessionId={}",
            self.player_state, self.idle_reason, self.session_id
        )
    }
}

struct LoadWaitError {
    user: String,
    log_tail: String,
}

fn load_reject_user(kind: &str) -> String {
    if kind == "LOAD_CANCELLED" {
        "The TV cancelled this slide.".to_string()
    } else {
        cast_panel_message(CastPanelCause::LoadError)
    }
}

/// `(requestId, type)` from a media payload. `requestId` is `None` when the
/// field is absent. An explicit `0` is unsolicited and must not satisfy a wait.
fn media_reply_type(msg: &CastMessage) -> Option<(Option<u32>, String)> {
    let CastMessagePayload::String(text) = &msg.payload else {
        return None;
    };
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let typ = value.get("type")?.as_str()?.to_string();
    let id = value.get("requestId").and_then(|v| v.as_u64()).map(|n| n as u32);
    Some((id, typ))
}

/// ERROR, LOAD_FAILED, and INVALID_REQUEST for this requestId.
/// rust_cast rejects an ERROR that has no detailedErrorCode, so this reads
/// the raw payload. requestId 0 does not match.
fn media_error_reply(msg: &CastMessage, id: u32) -> Option<&'static str> {
    let (rid, typ) = media_reply_type(msg)?;
    if id == 0 || rid != Some(id) {
        return None;
    }
    match typ.as_str() {
        "ERROR" => Some("ERROR"),
        "LOAD_FAILED" => Some("LOAD_FAILED"),
        "INVALID_REQUEST" => Some("INVALID_REQUEST"),
        _ => None,
    }
}

/// A parsed ERROR carries no request id in rust_cast. That satisfies a LOAD
/// wait only. PLAY, PAUSE, and SEEK ignore it and keep waiting. An explicit
/// requestId must match, and requestId 0 stays ignored.
fn parsed_error_matches(msg: &CastMessage, id: u32, allow_missing_request: bool) -> bool {
    match media_reply_type(msg) {
        Some((None, typ)) if typ == "ERROR" => allow_missing_request,
        Some((Some(rid), _)) => rid == id && id != 0,
        _ => false,
    }
}

/// The TV left PLAY or PAUSE unanswered. That is a warning, not a dead link.
fn media_cmd_unanswered(err: &str) -> bool {
    err == MEDIA_FETCH_MESSAGE || err.contains("didn't answer the media command")
}

fn idle_reason_token(reason: Option<IdleReason>) -> &'static str {
    match reason {
        Some(IdleReason::Cancelled) => "CANCELLED",
        Some(IdleReason::Interrupted) => "INTERRUPTED",
        Some(IdleReason::Finished) => "FINISHED",
        Some(IdleReason::Error) => "ERROR",
        None => "none",
    }
}

#[derive(Debug)]
enum Waited {
    Launched(Application),
    Loaded(LoadReply),
    LoadRejected { kind: &'static str },
    Media(PlayerState),
}

struct PumpUpdate {
    player: Option<PlayerState>,
    media_session: Option<i32>,
    idle_reason: Option<IdleReason>,
}

impl PumpUpdate {
    fn empty() -> Self {
        Self {
            player: None,
            media_session: None,
            idle_reason: None,
        }
    }
}

fn launch_payload(request_id: u32) -> String {
    serde_json::json!({
        "type": "LAUNCH",
        "requestId": request_id,
        "appId": "CC1AD845",
    })
    .to_string()
}

fn load_payload(
    request_id: u32,
    session_id: &str,
    url: &str,
    content_type: &str,
    autoplay: bool,
) -> String {
    serde_json::json!({
        "requestId": request_id,
        "sessionId": session_id,
        "type": "LOAD",
        "media": {
            "contentId": url,
            "streamType": "BUFFERED",
            "contentType": content_type,
        },
        "currentTime": 0.0,
        "autoplay": autoplay,
        "customData": {},
    })
    .to_string()
}

/// `still` and image content types are photos. Only a video is logged as video.
fn media_type_label(kind: &str) -> &'static str {
    match kind {
        "video" | "video/mp4" => "video",
        _ => "photo",
    }
}

/// `0` is not a page index. The log says `item=?` instead of inventing one.
fn item_log_token(item: u32) -> String {
    if item == 0 {
        "?".to_string()
    } else {
        item.to_string()
    }
}

fn load_log_detail(
    host: &str,
    request_id: u32,
    source: &str,
    item: u32,
    media_type: &str,
    url: &str,
    content_type: &str,
    tail: &str,
) -> String {
    let item = item_log_token(item);
    format!(
        "{host} requestId={request_id} source={source} item={item} type={media_type} url={} contentType={} {tail}",
        mask_load_url(url),
        log_ctype(content_type)
    )
}

/// The next playlist item a FINISHED status should LOAD.
struct Successor {
    url: String,
    content_type: String,
    item: u32,
    media_type: &'static str,
}

/// Host and port only. The path carries the media token and must not be logged.
fn media_url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.contains('@') || host.contains('\\') {
        return None;
    }
    Some(host.to_string())
}

fn map_launch_failure(err: &str) -> String {
    if is_cast_timeout(err) {
        LAUNCH_TIMEOUT_MESSAGE.to_string()
    } else {
        err.to_string()
    }
}

thread_local! {
    static CAST_LOG_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn cast_log_override_dir() -> Option<PathBuf> {
    CAST_LOG_OVERRIDE.with(|slot| slot.borrow().clone())
}

fn apply_cast_log_dir(dir: Option<PathBuf>) {
    if let Some(dir) = dir {
        CAST_LOG_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(dir));
    }
}

static CAST_LOG_LOCK: Mutex<()> = Mutex::new(());

pub fn cast_log_path() -> PathBuf {
    cast_log_dir().join("cast.log")
}

fn cast_log_dir() -> PathBuf {
    if let Some(dir) = CAST_LOG_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return dir;
    }
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) => PathBuf::from(local)
            .join("com.johnalindogan.slideshowpro")
            .join("logs"),
        None => std::env::temp_dir()
            .join("com.johnalindogan.slideshowpro")
            .join("logs"),
    }
}

fn cast_log_path_in(local_app_data: &Path) -> PathBuf {
    local_app_data
        .join("com.johnalindogan.slideshowpro")
        .join("logs")
        .join("cast.log")
}

fn cast_log(step: &str, detail: &str) {
    let _guard = CAST_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let safe = sanitize_cast_log_detail(detail);
    let line = format!("{} {step} {safe}", unix_to_utc(unix_now_secs()));
    let dir = cast_log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("cast.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len().saturating_add(line.len() as u64 + 1) > CAST_LOG_MAX {
            let previous = dir.join("cast.log.1");
            let _ = std::fs::remove_file(&previous);
            let _ = std::fs::rename(&path, &previous);
        }
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
    eprintln!("[cast] {step} {safe}");
}

fn sanitize_cast_log_detail(detail: &str) -> String {
    let flat: String = detail
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(360)
        .collect();
    if flat.contains('\\') || unmasked_media_token(&flat) {
        return "redacted".to_string();
    }
    let stripped = strip_allowed_log_slashes(&flat);
    if stripped.contains('/') {
        return "redacted".to_string();
    }
    flat
}

/// `/m/<token>/<key>` still has the token. `/m/<key>?t=***` does not.
fn unmasked_media_token(detail: &str) -> bool {
    let mut rest = detail;
    while let Some(at) = rest.find("/m/") {
        let after = &rest[at + 3..];
        let end = after.find([' ', '\t']).unwrap_or(after.len());
        let token = &after[..end];
        if let Some(slash) = token.find('/') {
            if !token[..slash].contains('?') {
                return true;
            }
        }
        if after.len() <= 1 {
            break;
        }
        rest = &after[1..];
    }
    false
}

fn strip_allowed_log_slashes(detail: &str) -> String {
    let mut out = detail.replace("https://", "https:").replace("http://", "http:");
    while let Some(start) = out.find("/m/") {
        let after = start + 3;
        let Some(mark) = out[after..].find("?t=***") else {
            return out;
        };
        let key = &out[after..after + mark];
        if !log_key_ok(key) {
            return out;
        }
        let end = after + mark + "?t=***".len();
        out.replace_range(start..end, "");
    }
    for ctype in ["image/jpeg", "image/png", "video/mp4"] {
        out = out.replace(ctype, "ctype");
    }
    out
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn unix_to_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn map_load_error(err: &str) -> String {
    let l = err.to_ascii_lowercase();
    if l.contains("fail") || l.contains("invalid") || l.contains("not supported") {
        UNSUPPORTED_MEDIA.to_string()
    } else if is_cast_timeout(err) || l.contains("did not start") {
        media_load_notice()
    } else {
        format!("The TV did not take this slide ({err}).")
    }
}

fn cast_err_is_timeout(err: &rust_cast::errors::Error) -> bool {
    use std::io::ErrorKind;
    match err {
        rust_cast::errors::Error::Io(io) => {
            matches!(io.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
                || is_cast_timeout(&io.to_string())
        }
        rust_cast::errors::Error::Timeout(_) => true,
        _ => is_cast_timeout(&err.to_string()),
    }
}

fn is_cast_timeout(err: &str) -> bool {
    let l = err.to_ascii_lowercase();
    l.contains("timed out")
        || l.contains("would block")
        || l.contains("timeout")
        || l.contains("10060")
        || l.contains("semaphore timeout")
}

fn link_down_message() -> String {
    "TV unreachable. It may be off or the Wi-Fi dropped. Cast again when it is back — you do not need to restart SlideX.".into()
}

/// Why the panel text changed. Connect failures stay "TV unreachable".
/// A live control channel that cannot fetch media, or that does not answer a
/// media command, uses [`MEDIA_FETCH_MESSAGE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastPanelCause {
    ConnectFailed,
    MediaCommandTimeout,
    LoadError,
}

pub fn cast_panel_message(cause: CastPanelCause) -> String {
    match cause {
        CastPanelCause::ConnectFailed => link_down_message(),
        CastPanelCause::MediaCommandTimeout | CastPanelCause::LoadError => {
            MEDIA_FETCH_MESSAGE.to_string()
        }
    }
}

fn player_name(state: PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "playing",
        PlayerState::Paused => "paused",
        PlayerState::Buffering => "buffering",
        PlayerState::Idle => "idle",
    }
}

struct SessionFlags {
    connected: bool,
    player_state: String,
    remote_event: Arc<Mutex<Option<String>>>,
    error: Option<String>,
    media_kind: String,
    local_cmd: Option<&'static str>,
    local_cmd_at: Instant,
    last_rx: Instant,
    media_session: Option<i32>,
    disconnect_origin: Option<CastOrigin>,
    /// mediaSessionId we already advanced from, so a repeated FINISHED is one step.
    advanced_session: Option<i32>,
    /// 1-based playlist index of the item now on the TV. 0 means we do not know.
    item_index: u32,
    /// mediaSessionId we already reported as a failed fetch, so IDLE/ERROR is one line.
    failed_session: Option<i32>,
    /// The page's play/pause, separate from the TV's playerState. Photo advances
    /// and a video FINISHED both wait on this. The TV status must not change it.
    slideshow_playing: bool,
    /// mediaSessionId whose photo FINISHED we already logged.
    ignored_photo: Option<i32>,
}

impl SessionFlags {
    /// Still images ignore `playerState`. The Default Media Receiver reports
    /// PAUSED for every photo, which is not a remote pause. A remote pause is
    /// a video going from PLAYING to PAUSED.
    fn note_state(&mut self, state: PlayerState, from_local: bool) {
        if self.media_kind == "still" && !from_local {
            return;
        }
        let name = player_name(state);
        let prev = self.player_state.clone();
        self.player_state = name.to_string();
        if from_local || name == prev {
            return;
        }
        if name == "paused" {
            if self.media_kind != "video" || prev != "playing" {
                return;
            }
        } else if name != "playing" {
            return;
        }
        let matches_local = self.local_cmd == Some(if name == "paused" { "pause" } else { "play" })
            && self.local_cmd_at.elapsed() < Duration::from_secs(2);
        if !matches_local {
            if let Ok(mut ev) = self.remote_event.lock() {
                *ev = Some(name.to_string());
            }
        }
    }

    /// IDLE + FINISHED for the item now on the TV. STOP, ERROR, and any other
    /// reason do not advance, and neither does a status for an older item.
    fn note_finished(&mut self, session: Option<i32>, reason: Option<IdleReason>) -> bool {
        let Some(session) = session else {
            return false;
        };
        let Some(current) = self.media_session else {
            return false;
        };
        if session != current || reason != Some(IdleReason::Finished) {
            return false;
        }
        if self.advanced_session == Some(session) {
            return false;
        }
        self.advanced_session = Some(session);
        cast_log("advance", "source=remote");
        true
    }

    /// IDLE/ERROR for the item now on the TV. A stale session is ignored.
    fn note_load_failed(&mut self, session: i32) -> bool {
        if self.failed_session == Some(session) {
            return false;
        }
        self.failed_session = Some(session);
        let item = item_log_token(self.item_index);
        cast_log("load_failed", &format!("item={item} reason=ERROR"));
        self.error = Some(cast_panel_message(CastPanelCause::LoadError));
        if self.player_state == "playing" {
            self.player_state = "idle".into();
        }
        if let Ok(mut ev) = self.remote_event.lock() {
            *ev = Some("load_failed".to_string());
        }
        true
    }

    /// Receiver FINISHED on a still. Logged once per media session, then ignored.
    fn note_photo_finished_ignored(&mut self, session: i32) {
        if self.ignored_photo == Some(session) {
            return;
        }
        self.ignored_photo = Some(session);
        let item = item_log_token(self.item_index);
        cast_log("photo_finished_ignored", &format!("item={item}"));
    }
}

/// Side effects of an IDLE status, up to the point where the next LOAD is
/// chosen. `true` means the caller should load a successor.
fn idle_wants_next(
    flags: &mut SessionFlags,
    session: Option<i32>,
    reason: Option<IdleReason>,
) -> bool {
    let Some(session) = session else {
        return false;
    };
    let Some(current) = flags.media_session else {
        return false;
    };
    if session != current {
        return false;
    }
    if reason == Some(IdleReason::Error) {
        flags.note_load_failed(session);
        return false;
    }
    if flags.media_kind == "still" && reason == Some(IdleReason::Finished) {
        flags.note_photo_finished_ignored(session);
        return false;
    }
    if reason == Some(IdleReason::Finished) && !flags.slideshow_playing {
        return false;
    }
    flags.note_finished(Some(session), reason)
}

fn apply_successor(
    link: &CastLink,
    flags: &mut SessionFlags,
    next: Result<Successor, &'static str>,
) -> Result<bool, String> {
    let next = match next {
        Ok(next) => next,
        Err(why) => {
            let item = item_log_token(flags.item_index);
            cast_log("load_skipped", &format!("item={item} reason={why}"));
            return Ok(false);
        }
    };
    match link.load_url(
        &next.url,
        &next.content_type,
        true,
        CastOrigin::Remote,
        next.item,
        next.media_type,
    ) {
        Ok(sid) => {
            flags.media_session = Some(sid);
            flags.advanced_session = None;
            flags.failed_session = None;
            flags.ignored_photo = None;
            flags.item_index = next.item;
            flags.media_kind = if next.media_type == "video" {
                "video"
            } else {
                "still"
            }
            .into();
            flags.player_state = "playing".into();
            flags.error = None;
            flags.local_cmd = Some("play");
            flags.local_cmd_at = Instant::now();
            Ok(true)
        }
        Err(err) if err == CAST_CANCELLED => {
            cast_log(
                "load_skipped",
                &format!("item={} reason=cast stopped", item_log_token(next.item)),
            );
            Err(err)
        }
        Err(err) => {
            if !err.contains("unreachable") {
                flags.error = Some(err.clone());
                if flags.player_state == "playing" {
                    flags.player_state = "idle".into();
                }
            }
            Err(err)
        }
    }
}

/// IDLE for the current media session.
/// A video FINISHED loads `next` while the slideshow is playing, or logs why
/// that LOAD was not sent. A photo FINISHED is logged and ignored: the
/// slideshow timer is the only photo advance. ERROR is a failed fetch: tell
/// the user and do not skip to the next slide.
/// `Ok(true)` means the next LOAD was sent. The caller publishes `item_index`
/// before raising the finished event, so the page moves to that item.
fn on_media_idle(
    link: &CastLink,
    flags: &mut SessionFlags,
    session: Option<i32>,
    reason: Option<IdleReason>,
    next: Result<Successor, &'static str>,
) -> Result<bool, String> {
    if !idle_wants_next(flags, session, reason) {
        return Ok(false);
    }
    apply_successor(link, flags, next)
}

enum CastCmd {
    SetPlaylist {
        paths: Vec<PathBuf>,
        reply: Sender<Result<PlaylistUpdate, String>>,
    },
    Load {
        path: PathBuf,
        autoplay: bool,
        origin: CastOrigin,
        /// Photo hold from the page timer. Ignored unless `origin` is timer.
        duration_ms: Option<u64>,
        reply: Sender<Result<(), String>>,
    },
    Pause {
        origin: CastOrigin,
        reply: Sender<Result<(), String>>,
    },
    Play {
        origin: CastOrigin,
        reply: Sender<Result<(), String>>,
    },
    Disconnect {
        origin: CastOrigin,
        reply: Sender<Result<(), String>>,
    },
}

fn reject_cancelled(cmd: CastCmd) {
    let err = CAST_CANCELLED.to_string();
    match cmd {
        CastCmd::SetPlaylist { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        CastCmd::Load { reply, .. }
        | CastCmd::Pause { reply, .. }
        | CastCmd::Play { reply, .. }
        | CastCmd::Disconnect { reply, .. } => {
            let _ = reply.send(Err(err));
        }
    }
}

struct LiveSession {
    cmd_tx: Sender<CastCmd>,
    status: Arc<Mutex<CastLiveStatus>>,
    remote_event: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    disconnect_logged: Arc<AtomicBool>,
}

pub struct CastState {
    inner: Mutex<Option<LiveSession>>,
    /// Connect failures are not stored as a session. status() still reports them.
    last_error: Mutex<Option<String>>,
}

impl Default for CastState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
            last_error: Mutex::new(None),
        }
    }
}

impl CastState {
    pub fn connect(
        &self,
        host: &str,
        port: Option<u16>,
        name: Option<String>,
    ) -> Result<CastConnectInfo, String> {
        self.connect_wait(host, port, name, SESSION_READY_WAIT)
    }

    fn connect_wait(
        &self,
        host: &str,
        port: Option<u16>,
        name: Option<String>,
        ready_wait: Duration,
    ) -> Result<CastConnectInfo, String> {
        match self.connect_wait_inner(host, port, name, ready_wait) {
            Ok(info) => {
                if let Ok(mut slot) = self.last_error.lock() {
                    *slot = None;
                }
                Ok(info)
            }
            Err(err) => {
                if let Ok(mut slot) = self.last_error.lock() {
                    *slot = Some(err.clone());
                }
                Err(err)
            }
        }
    }

    fn connect_wait_inner(
        &self,
        host: &str,
        port: Option<u16>,
        name: Option<String>,
        ready_wait: Duration,
    ) -> Result<CastConnectInfo, String> {
        // The profile query runs on its own thread. Do not wait for it here.
        let _ = lan_category_now();
        let (ip, port) = parse_cast_host(host, port)?;
        let snap = snapshot_ifaces();
        let bind = choose_bind_for_device(&snap.preferred, ip)
            .cloned()
            .ok_or_else(|| NO_HOME_LAN_MESSAGE.to_string())?;
        // Replacing a session that was never opened is not a user Disconnect.
        self.disconnect_session()?;

        let device_name = name.unwrap_or_else(|| "Chromecast".into());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(CastLiveStatus {
            connected: false,
            player_state: "idle".into(),
            remote_event: None,
            error: None,
            media_kind: String::new(),
            http_port: 0,
            bind_ip: bind.ip.to_string(),
            item_index: 0,
        }));
        let remote_event = Arc::new(Mutex::new(None));
        let status_t = Arc::clone(&status);
        let remote_t = Arc::clone(&remote_event);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_t = Arc::clone(&cancel);
        let disconnect_logged = Arc::new(AtomicBool::new(false));
        let logged_t = Arc::clone(&disconnect_logged);
        let friendly = device_name.clone();
        let worker = thread::Builder::new()
            .name("cast-session".into())
            .spawn(move || {
                run_session(
                    ip, port, friendly, bind.ip, cmd_rx, ready_tx, status_t, remote_t, cancel_t,
                    logged_t,
                );
            })
            .map_err(|e| format!("cast thread: {e}"))?;

        let info = match ready_rx.recv_timeout(ready_wait) {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                let _ = worker.join();
                return Err(with_public_network(e));
            }
            Err(_) => {
                cancel.store(true, Ordering::SeqCst);
                let _ = cmd_tx.send(CastCmd::Disconnect {
                    origin: CastOrigin::Timeout,
                    reply: mpsc::channel().0,
                });
                // The worker is often still inside TLS or launch and exits
                // before it reads Disconnect, so the caller records the line.
                log_disconnect_once(&disconnect_logged, "source=timeout");
                let _ = worker.join();
                let reason = with_public_network(link_down_message());
                log_connect_error(&reason, ip);
                return Err(reason);
            }
        };

        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "cast state lock".to_string())?;
        *guard = Some(LiveSession {
            cmd_tx,
            status,
            remote_event,
            worker: Some(worker),
            cancel,
            disconnect_logged,
        });
        Ok(info)
    }

    pub fn set_playlist(&self, paths: Vec<String>) -> Result<PlaylistUpdate, String> {
        let bufs = paths.iter().map(PathBuf::from).collect();
        self.roundtrip(|tx| {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(CastCmd::SetPlaylist {
                paths: bufs,
                reply: reply_tx,
            })
            .map_err(|_| "Cast is not connected.".to_string())?;
            reply_rx
                .recv_timeout(Duration::from_secs(3))
                .map_err(|_| "Updating the playlist on the TV timed out.".to_string())?
        })
    }

    pub fn load(
        &self,
        path: String,
        autoplay: bool,
        origin: CastOrigin,
        duration_ms: Option<u64>,
    ) -> Result<(), String> {
        self.roundtrip(|tx| {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(CastCmd::Load {
                path: PathBuf::from(path),
                autoplay,
                origin,
                duration_ms,
                reply: reply_tx,
            })
            .map_err(|_| "Cast is not connected.".to_string())?;
            reply_rx
                .recv_timeout(Duration::from_secs(16))
                .map_err(|_| media_load_notice())?
        })
    }

    pub fn pause(&self, origin: CastOrigin) -> Result<(), String> {
        self.roundtrip(|tx| {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(CastCmd::Pause {
                origin,
                reply: reply_tx,
            })
            .map_err(|_| "Cast is not connected.".to_string())?;
            reply_rx
                .recv_timeout(MEDIA_CMD_REPLY_WAIT)
                .map_err(|_| cast_panel_message(CastPanelCause::MediaCommandTimeout))?
        })
    }

    pub fn play(&self, origin: CastOrigin) -> Result<(), String> {
        self.roundtrip(|tx| {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(CastCmd::Play {
                origin,
                reply: reply_tx,
            })
            .map_err(|_| "Cast is not connected.".to_string())?;
            reply_rx
                .recv_timeout(MEDIA_CMD_REPLY_WAIT)
                .map_err(|_| cast_panel_message(CastPanelCause::MediaCommandTimeout))?
        })
    }

    pub fn disconnect(&self) -> Result<(), String> {
        self.disconnect_session()
    }

    /// Drop the live session. A Disconnect with nothing connected writes no line.
    fn disconnect_session(&self) -> Result<(), String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "cast state lock".to_string())?;
        let Some(mut live) = guard.take() else {
            return Ok(());
        };
        // Cancel before the Disconnect command so a queued slideshow LOAD is dropped.
        live.cancel.store(true, Ordering::SeqCst);
        let logged = Arc::clone(&live.disconnect_logged);
        let (reply_tx, reply_rx) = mpsc::channel();
        if live
            .cmd_tx
            .send(CastCmd::Disconnect {
                origin: CastOrigin::User,
                reply: reply_tx,
            })
            .is_err()
        {
            log_disconnect_once(&logged, "source=user error=not connected");
            return Ok(());
        }
        drop(guard);
        let replied = reply_rx.recv_timeout(Duration::from_secs(3));
        if let Some(worker) = live.worker.take() {
            let _ = worker.join();
        }
        if replied.is_err() {
            log_disconnect_once(&logged, "source=user error=no reply");
        }
        Ok(())
    }

    pub fn status(&self) -> CastLiveStatus {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                return CastLiveStatus {
                    connected: false,
                    player_state: "idle".into(),
                    remote_event: None,
                    error: Some("Cast status is unavailable.".into()),
                    media_kind: String::new(),
                    http_port: 0,
                    bind_ip: String::new(),
                    item_index: 0,
                };
            }
        };
        let Some(live) = guard.as_ref() else {
            let error = self.last_error.lock().ok().and_then(|slot| slot.clone());
            return CastLiveStatus {
                connected: false,
                player_state: "idle".into(),
                remote_event: None,
                error,
                media_kind: String::new(),
                http_port: 0,
                bind_ip: String::new(),
                item_index: 0,
            };
        };
        let mut status = live
            .status
            .lock()
            .map(|g| g.clone())
            .unwrap_or(CastLiveStatus {
                connected: false,
                player_state: "idle".into(),
                remote_event: None,
                error: Some(link_down_message()),
                media_kind: String::new(),
                http_port: 0,
                bind_ip: String::new(),
                item_index: 0,
            });
        status.remote_event = live.remote_event.lock().ok().and_then(|mut g| g.take());
        status
    }

    pub fn shutdown(&self) {
        let _ = self.disconnect();
    }

    fn roundtrip<T>(
        &self,
        f: impl FnOnce(&Sender<CastCmd>) -> Result<T, String>,
    ) -> Result<T, String> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| "cast state lock".to_string())?;
        let live = guard
            .as_ref()
            .ok_or_else(|| "Cast is not connected.".to_string())?;
        f(&live.cmd_tx)
    }
}

impl Drop for CastState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn publish(status: &Mutex<CastLiveStatus>, edit: impl FnOnce(&mut CastLiveStatus)) {
    if let Ok(mut g) = status.lock() {
        edit(&mut g);
    }
}

/// A media-command timeout or load error is visible on `cast_status`.
/// `player_state` is not left at `playing`. `connected` stays true unless this
/// failure is what tore the session down.
fn publish_media_failure(
    flags: &mut SessionFlags,
    status: &Mutex<CastLiveStatus>,
    err: &str,
    torn_down: bool,
) {
    flags.error = Some(err.to_string());
    if flags.player_state == "playing" {
        flags.player_state = "idle".into();
    }
    if torn_down {
        flags.connected = false;
    }
    let player = flags.player_state.clone();
    let connected = flags.connected;
    let error = flags.error.clone();
    publish(status, |s| {
        s.error = error;
        s.player_state = player;
        s.connected = connected;
    });
}

fn finish_media_cmd(
    result: Result<(), String>,
    reply: Sender<Result<(), String>>,
    flags: &mut SessionFlags,
    status: &Mutex<CastLiveStatus>,
) -> bool {
    let torn = result.as_ref().err().is_some_and(|err| {
        err != CAST_CANCELLED && err.contains("TV unreachable")
    });
    if let Err(err) = result.as_ref() {
        if err != CAST_CANCELLED {
            let shown = if torn {
                link_down_message()
            } else {
                err.clone()
            };
            publish_media_failure(flags, status, &shown, torn);
        }
    }
    let _ = reply.send(result);
    !torn
}

fn run_session(
    host: Ipv4Addr,
    port: u16,
    device_name: String,
    bind_ip: Ipv4Addr,
    cmd_rx: Receiver<CastCmd>,
    ready_tx: Sender<Result<CastConnectInfo, String>>,
    status: Arc<Mutex<CastLiveStatus>>,
    remote_event: Arc<Mutex<Option<String>>>,
    cancel: Arc<AtomicBool>,
    disconnect_logged: Arc<AtomicBool>,
) {
    // Start the profile query without waiting. A later load timeout only reads the cache.
    let _ = lan_category_now();
    let mut http = match MediaHttp::start_lan(bind_ip) {
        Ok(h) => h,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let mut link = match CastLink::connect(host, port) {
        Ok(mut l) => {
            l.use_cancel(cancel);
            l.use_disconnect_logged(disconnect_logged);
            l
        }
        Err(e) => {
            http.shutdown();
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    if let Err(e) = link.launch_receiver() {
        http.shutdown();
        let _ = ready_tx.send(Err(e));
        return;
    }

    let info = CastConnectInfo {
        device_name: device_name.clone(),
        device_host: host.to_string(),
        device_port: port,
        bind_ip: bind_ip.to_string(),
        http_port: http.port,
    };
    publish(&status, |s| {
        s.connected = true;
        s.http_port = http.port;
        s.bind_ip = bind_ip.to_string();
        s.error = None;
    });
    let _ = ready_tx.send(Ok(info));
    eprintln!("[cast] session up, media port {}", http.port);

    let mut flags = SessionFlags {
        connected: true,
        player_state: "idle".into(),
        remote_event,
        error: None,
        media_kind: String::new(),
        local_cmd: None,
        local_cmd_at: Instant::now(),
        last_rx: Instant::now(),
        media_session: None,
        disconnect_origin: None,
        advanced_session: None,
        item_index: 0,
        failed_session: None,
        slideshow_playing: true,
        ignored_photo: None,
    };
    let mut last_ping = Instant::now();
    let mut running = true;

    while running {
        if flags.last_rx.elapsed() > LINK_DEAD_AFTER {
            cast_log("timeout", "no message from the TV for 15s");
            flags.connected = false;
            flags.error = Some(link_down_message());
            publish(&status, |s| {
                s.connected = false;
                s.error = Some(link_down_message());
            });
            fail_pending(&cmd_rx, &link_down_message(), &link);
            break;
        }

        match cmd_rx.try_recv() {
            Ok(cmd) => {
                if link.cancelled() && !matches!(cmd, CastCmd::Disconnect { .. }) {
                    reject_cancelled(cmd);
                    continue;
                }
                if !handle_cmd(cmd, &mut http, &link, &mut flags, &status, &mut running) {
                    running = false;
                }
                flags.last_rx = Instant::now();
                continue;
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }

        if last_ping.elapsed() >= PING_EVERY {
            last_ping = Instant::now();
            if link.ping().is_err() {
                flags.connected = false;
                flags.error = Some(link_down_message());
                publish(&status, |s| {
                    s.connected = false;
                    s.error = Some(link_down_message());
                });
                fail_pending(&cmd_rx, &link_down_message(), &link);
                break;
            }
        }

        match link.pump() {
            Ok(Some(update)) => {
                flags.last_rx = Instant::now();
                if let Some(state) = update.player {
                    flags.note_state(state, false);
                    let mut advanced = false;
                    if state == PlayerState::Idle {
                        if idle_wants_next(&mut flags, update.media_session, update.idle_reason) {
                            let (next, skipped_video) = http.next_castable(flags.item_index);
                            match apply_successor(&link, &mut flags, next) {
                                Ok(did) => {
                                    advanced = did;
                                    if let Some(msg) = skipped_video {
                                        flags.error = Some(msg);
                                    }
                                }
                                Err(err) if err.contains("TV unreachable") => {
                                    flags.connected = false;
                                    flags.error = Some(link_down_message());
                                    publish(&status, |s| {
                                        s.connected = false;
                                        s.error = Some(link_down_message());
                                    });
                                    fail_pending(&cmd_rx, &err, &link);
                                    break;
                                }
                                Err(_) => {}
                            }
                        }
                    }
                    let name = flags.player_state.clone();
                    let err = flags.error.clone();
                    let kind = flags.media_kind.clone();
                    let item_index = flags.item_index;
                    publish(&status, |s| {
                        s.player_state = name;
                        s.media_kind = kind;
                        s.connected = flags.connected;
                        s.error = err;
                        s.item_index = item_index;
                    });
                    if advanced {
                        if let Ok(mut ev) = flags.remote_event.lock() {
                            *ev = Some("finished".to_string());
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                flags.connected = false;
                flags.error = Some(e.clone());
                publish(&status, |s| {
                    s.connected = false;
                    s.error = Some(e.clone());
                });
                fail_pending(&cmd_rx, &e, &link);
                break;
            }
        }
    }

    if let Some(origin) = flags.disconnect_origin {
        link.finish_disconnect(flags.media_session, origin);
    }
    http.shutdown();
    publish(&status, |s| {
        s.connected = false;
        if s.error.is_none() {
            s.error = flags.error.clone();
        }
    });
    eprintln!("[cast] session ended");
}

fn fail_pending(cmd_rx: &Receiver<CastCmd>, err: &str, link: &CastLink) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            CastCmd::SetPlaylist { reply, .. } => {
                let _ = reply.send(Err(err.to_string()));
            }
            CastCmd::Load { reply, .. }
            | CastCmd::Pause { reply, .. }
            | CastCmd::Play { reply, .. } => {
                let _ = reply.send(Err(err.to_string()));
            }
            CastCmd::Disconnect { origin, reply } => {
                link.log_disconnect(&format!("source={} error=link down", origin.as_str()));
                let _ = reply.send(Err(err.to_string()));
            }
        }
    }
}

fn handle_cmd(
    cmd: CastCmd,
    http: &mut MediaHttp,
    link: &CastLink,
    flags: &mut SessionFlags,
    status: &Mutex<CastLiveStatus>,
    running: &mut bool,
) -> bool {
    match cmd {
        CastCmd::SetPlaylist { paths, reply } => {
            let result = http.set_playlist(&paths);
            let _ = reply.send(result);
            true
        }
        CastCmd::Load {
            path,
            autoplay,
            origin,
            duration_ms,
            reply,
        } => {
            let result = (|| {
                if link.cancelled() {
                    return Err(CAST_CANCELLED.to_string());
                }
                let (url, ctype) = http.media_url(&path)?;
                let kind = http.kind_of(&path)?;
                let item = http.index_of(&path).unwrap_or(0);
                if kind == "video" {
                    let decision = decide_cast_video(&path);
                    if let Some(level) = decision.derived.as_deref() {
                        cast_log(
                            "level_derived",
                            &format!("item={} level_derived={level}", item_log_token(item)),
                        );
                    }
                    if let Some(reasons) = decision.skip {
                        cast_log(
                            "video_skipped",
                            &format!("item={} reasons={reasons}", item_log_token(item)),
                        );
                        return Err(reasons);
                    }
                }
                let play = if kind == "still" { true } else { autoplay };
                link.photo_timer_ms.set(if origin == CastOrigin::Timer {
                    duration_ms
                } else {
                    None
                });
                let sid = link.load_url(&url, &ctype, play, origin, item, &kind)?;
                flags.media_session = Some(sid);
                flags.advanced_session = None;
                flags.failed_session = None;
                flags.ignored_photo = None;
                flags.slideshow_playing = autoplay;
                flags.item_index = item;
                flags.error = None;
                if let Ok(mut ev) = flags.remote_event.lock() {
                    if ev.as_deref() == Some("finished") {
                        *ev = None;
                    }
                }
                flags.media_kind = kind.clone();
                flags.player_state = if play { "playing" } else { "paused" }.into();
                flags.local_cmd = Some(if play { "play" } else { "pause" });
                flags.local_cmd_at = Instant::now();
                flags.last_rx = Instant::now();
                publish(status, |s| {
                    s.connected = true;
                    s.media_kind = kind;
                    s.player_state = flags.player_state.clone();
                    s.error = None;
                    s.item_index = flags.item_index;
                });
                eprintln!("[cast] loaded {}", flags.media_kind);
                Ok(())
            })();
            finish_media_cmd(result, reply, flags, status)
        }
        CastCmd::Pause { origin, reply } => {
            flags.local_cmd = Some("pause");
            flags.local_cmd_at = Instant::now();
            let result = (|| {
                let sid = flags
                    .media_session
                    .ok_or_else(|| "Nothing is casting.".to_string())?;
                if flags.media_kind == "still" {
                    // The receiver reports a still as PAUSED or BUFFERING and
                    // does not answer PAUSE. Waiting out the media deadline
                    // makes the panel say the TV never loaded the photo.
                    let _ = (sid, origin);
                    flags.player_state = "paused".into();
                    flags.slideshow_playing = false;
                    publish(status, |s| s.player_state = "paused".into());
                    return Ok(());
                }
                link.pause(sid, origin)?;
                flags.player_state = "paused".into();
                flags.slideshow_playing = false;
                flags.last_rx = Instant::now();
                publish(status, |s| s.player_state = "paused".into());
                Ok(())
            })();
            finish_media_cmd(result, reply, flags, status)
        }
        CastCmd::Play { origin, reply } => {
            flags.local_cmd = Some("play");
            flags.local_cmd_at = Instant::now();
            let result = (|| {
                let sid = flags
                    .media_session
                    .ok_or_else(|| "Nothing is casting.".to_string())?;
                // LOAD already carries autoplay. PLAY is only a resume from
                // pause, and never for a photo: the TV does not answer it.
                if flags.media_kind == "still" || flags.player_state != "paused" {
                    let reason = if flags.media_kind == "still" {
                        "photo"
                    } else {
                        "not_paused"
                    };
                    cast_log("play_skipped", &format!("reason={reason}"));
                    let _ = (sid, origin);
                    flags.player_state = "playing".into();
                    flags.slideshow_playing = true;
                    publish(status, |s| s.player_state = "playing".into());
                    return Ok(());
                }
                link.play(sid, origin)?;
                flags.player_state = "playing".into();
                flags.slideshow_playing = true;
                flags.last_rx = Instant::now();
                publish(status, |s| s.player_state = "playing".into());
                Ok(())
            })();
            finish_media_cmd(result, reply, flags, status)
        }
        CastCmd::Disconnect { origin, reply } => {
            *running = false;
            flags.disconnect_origin = Some(origin);
            flags.connected = false;
            publish(status, |s| {
                s.connected = false;
                s.error = None;
            });
            let _ = reply.send(Ok(()));
            false
        }
    }
}

const FW_MEDIA_RULE: &str = "SlideX Cast media (Private)";
const FW_MDNS_RULE: &str = "SlideX Cast mDNS (Private)";

/// `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`. Never `powershell` from PATH.
pub fn powershell_exe_under_windows_dir(windows_dir: &str) -> String {
    let root = windows_dir.trim_end_matches(['\\', '/']);
    format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe")
}

#[cfg(windows)]
fn windows_dir_from_env() -> String {
    std::env::var("SystemRoot")
        .or_else(|_| std::env::var("WINDIR"))
        .unwrap_or_else(|_| r"C:\Windows".to_string())
}

/// Quote one PowerShell `-File` argument. Apostrophes stay inside double quotes
/// so a folder named O'Brien is not treated as a string delimiter.
pub fn quote_powershell_arg(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(|c| matches!(c, ' ' | '\t' | '"' | '\'')) {
        format!("\"{}\"", arg.replace('"', "\"\""))
    } else {
        arg.to_string()
    }
}

/// Args for `powershell.exe -File cast-fw-check.ps1`. The exe path is its own
/// argument, never interpolated into a quoted script string.
pub fn firewall_powershell_file_args(script: &Path, exe: &str) -> Vec<String> {
    vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-ExecutionPolicy".into(),
        "Bypass".into(),
        "-File".into(),
        script.display().to_string(),
        "-ExePath".into(),
        exe.to_string(),
        "-MediaName".into(),
        FW_MEDIA_RULE.into(),
        "-MdnsName".into(),
        FW_MDNS_RULE.into(),
        "-TcpPorts".into(),
        format!("{CAST_PORT_LO}-{CAST_PORT_HI}"),
        "-UdpPort".into(),
        "5353".into(),
    ]
}

const FW_SCRIPT_MISSING: &str = "The firewall check script cast-fw-check.ps1 is not installed beside the app. Re-run the SlideX installer.";
const FW_SCRIPT_MISSING_QUERY: &str = "query failed: cast-fw-check.ps1 is not installed";

#[cfg(windows)]
fn cast_fw_check_script() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join("cast-fw-check.ps1");
            if beside.is_file() {
                return Some(beside);
            }
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("windows")
        .join("cast-fw-check.ps1");
    dev.is_file().then_some(dev)
}

pub fn firewall_status() -> CastFirewallStatus {
    #[cfg(windows)]
    {
        return parse_firewall_script_output(&query_firewall_text());
    }
    #[cfg(not(windows))]
    {
        CastFirewallStatus {
            state: "unknown".into(),
            detail: format!(
                "On Windows the installer adds \"{FW_MEDIA_RULE}\" and \"{FW_MDNS_RULE}\" for slideshowpro.exe: TCP {CAST_PORT_LO}-{CAST_PORT_HI} and UDP 5353, Private profile only. This check runs on Windows."
            ),
        }
    }
}

#[cfg(windows)]
fn query_firewall_text() -> String {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return format!("query failed: {e}"),
    };
    let Some(script) = cast_fw_check_script() else {
        eprintln!("[cast] {FW_SCRIPT_MISSING}");
        return FW_SCRIPT_MISSING_QUERY.into();
    };
    let program = powershell_exe_under_windows_dir(&windows_dir_from_env());
    let mut cmd = std::process::Command::new(&program);
    {
        use std::os::windows::process::CommandExt;
        for arg in firewall_powershell_file_args(&script, &exe) {
            cmd.raw_arg(quote_powershell_arg(&arg));
        }
    }
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            if !out.status.success() && !stdout.to_ascii_lowercase().contains("status=") {
                let err = String::from_utf8_lossy(&out.stderr);
                format!("query failed: {err}\n{stdout}")
            } else {
                stdout
            }
        }
        Err(e) => format!("query failed: {e}"),
    }
}

pub fn parse_firewall_script_output(text: &str) -> CastFirewallStatus {
    let lower = text.to_ascii_lowercase();
    if lower.contains("status=match") {
        CastFirewallStatus {
            state: "added".into(),
            detail: format!(
                "Private firewall rules are installed for slideshowpro.exe: TCP {CAST_PORT_LO}-{CAST_PORT_HI} and UDP 5353."
            ),
        }
    } else if lower.contains("status=partial") {
        CastFirewallStatus {
            state: "partial".into(),
            detail: "Only part of the Cast firewall rule is installed. Re-run the SlideX installer and accept the Windows prompt.".into(),
        }
    } else if lower.contains("query failed") {
        CastFirewallStatus {
            state: "unknown".into(),
            detail: firewall_query_failure_detail(text).into(),
        }
    } else {
        CastFirewallStatus {
            state: "missing".into(),
            detail: format!(
                "No Private inbound rule for slideshowpro.exe. Cast needs TCP {CAST_PORT_LO}-{CAST_PORT_HI} and UDP 5353 on private networks. Re-run the installer and accept the Windows prompt."
            ),
        }
    }
}

pub fn parse_firewall_report(text: &str, exe: &str) -> CastFirewallStatus {
    let ports = format!("{CAST_PORT_LO}-{CAST_PORT_HI}");
    let mut media_ok = false;
    let mut mdns_ok = false;
    let mut saw_json = false;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        saw_json = true;
        if firewall_json_matches(&value, exe, FW_MEDIA_RULE, "TCP", &ports) {
            media_ok = true;
        }
        if firewall_json_matches(&value, exe, FW_MDNS_RULE, "UDP", "5353") {
            mdns_ok = true;
        }
    }
    if media_ok && mdns_ok {
        CastFirewallStatus {
            state: "added".into(),
            detail: format!(
                "Private firewall rules are installed for slideshowpro.exe: TCP {CAST_PORT_LO}-{CAST_PORT_HI} and UDP 5353."
            ),
        }
    } else if media_ok || mdns_ok {
        CastFirewallStatus {
            state: "partial".into(),
            detail: "Only part of the Cast firewall rule is installed. Re-run the SlideX installer and accept the Windows prompt.".into(),
        }
    } else if !saw_json && text.to_ascii_lowercase().contains("query failed") {
        CastFirewallStatus {
            state: "unknown".into(),
            detail: firewall_query_failure_detail(text).into(),
        }
    } else {
        CastFirewallStatus {
            state: "missing".into(),
            detail: format!(
                "No Private inbound rule for slideshowpro.exe. Cast needs TCP {CAST_PORT_LO}-{CAST_PORT_HI} and UDP 5353 on private networks. Re-run the installer and accept the Windows prompt."
            ),
        }
    }
}

fn firewall_query_failure_detail(text: &str) -> &'static str {
    let lower = text.to_ascii_lowercase();
    if lower.contains("cast-fw-check.ps1") && lower.contains("not installed") {
        FW_SCRIPT_MISSING
    } else {
        "Could not query the Windows firewall."
    }
}

fn json_field(value: &serde_json::Value, key: &str) -> String {
    match value.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn field_enabled(value: &serde_json::Value) -> bool {
    match value.get("enabled") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => {
            matches!(s.to_ascii_lowercase().as_str(), "true" | "yes")
        }
        _ => false,
    }
}

fn normalize_program_path(path: &str) -> String {
    let mut p = path.trim().replace('/', "\\");
    let lower = p.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") {
        p = format!(r"\\{}", &p[8..]);
    } else if lower.starts_with(r"\\?\") {
        let rest = &p[4..];
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            p = rest.to_string();
        }
    }
    while p.ends_with('\\') && p.len() > 3 {
        p.pop();
    }
    p.to_ascii_lowercase()
}

fn profile_includes_private(profile: &str) -> bool {
    let p = profile.trim().to_ascii_lowercase();
    if p == "any" {
        return true;
    }
    p.split(|c: char| c == ',' || c.is_whitespace())
        .any(|part| part == "private" || part == "2")
}

fn remote_has_local_subnet(remote: &str) -> bool {
    remote
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|part| part.eq_ignore_ascii_case("LocalSubnet"))
}

fn protocol_match(got: &str, expect: &str) -> bool {
    let got = got.trim();
    if got.eq_ignore_ascii_case(expect) {
        return true;
    }
    (expect.eq_ignore_ascii_case("TCP") && got == "6")
        || (expect.eq_ignore_ascii_case("UDP") && got == "17")
}

fn ports_match(got: &str, expect: &str) -> bool {
    let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    compact(got).eq_ignore_ascii_case(&compact(expect))
}

fn firewall_json_matches(
    value: &serde_json::Value,
    exe: &str,
    name: &str,
    protocol: &str,
    ports: &str,
) -> bool {
    if !json_field(value, "name").eq_ignore_ascii_case(name) {
        return false;
    }
    if !field_enabled(value) {
        return false;
    }
    let direction = json_field(value, "direction");
    if !direction.eq_ignore_ascii_case("Inbound") && !direction.eq_ignore_ascii_case("In") {
        return false;
    }
    if !json_field(value, "action").eq_ignore_ascii_case("Allow") {
        return false;
    }
    if normalize_program_path(&json_field(value, "program")) != normalize_program_path(exe) {
        return false;
    }
    if !profile_includes_private(&json_field(value, "profile")) {
        return false;
    }
    if !remote_has_local_subnet(&json_field(value, "remote")) {
        return false;
    }
    protocol_match(&json_field(value, "protocol"), protocol)
        && ports_match(&json_field(value, "localport"), ports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn cand(name: &str, ip: [u8; 4], prefix: u8) -> IfaceCand {
        IfaceCand {
            name: name.into(),
            ip: Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]),
            prefix,
        }
    }

    #[test]
    fn bind_prefers_home_wifi_over_tailscale_and_public() {
        let ifaces = vec![
            cand("Tailscale", [100, 64, 0, 5], 32),
            cand("Ethernet", [8, 8, 8, 8], 24),
            cand("Wi-Fi", [192, 168, 1, 20], 24),
            cand("vEthernet", [169, 254, 1, 1], 16),
            cand("Tailscale", [192, 168, 50, 2], 32),
        ];
        let pick = choose_bind_ip(&ifaces, Some(Ipv4Addr::new(100, 64, 0, 5))).unwrap();
        assert_eq!(pick.ip, Ipv4Addr::new(192, 168, 1, 20));
        assert!(iface_role("Tailscale", Ipv4Addr::new(100, 64, 0, 5)) == IfaceRole::Skip);
        assert!(iface_role("eth0", Ipv4Addr::new(169, 254, 3, 4)) == IfaceRole::Skip);
        assert!(iface_role("eth0", Ipv4Addr::UNSPECIFIED) == IfaceRole::Skip);
        assert!(!is_safe_bind_ip(Ipv4Addr::UNSPECIFIED));
        assert!(!is_safe_bind_ip(Ipv4Addr::new(1, 2, 3, 4)));
        assert!(is_safe_bind_ip(Ipv4Addr::new(10, 1, 2, 3)));
        assert_eq!(
            iface_role("NordLynx", Ipv4Addr::new(10, 5, 0, 2)),
            IfaceRole::Skip
        );
        assert_eq!(
            iface_role("vEthernet (Default Switch)", Ipv4Addr::new(172, 22, 80, 1)),
            IfaceRole::Skip
        );
    }

    #[test]
    fn nordlynx_default_route_does_not_beat_wifi() {
        let ifaces = vec![
            cand("NordLynx", [10, 5, 0, 2], 32),
            cand("Wi-Fi", [192, 168, 1, 20], 24),
        ];
        let pick = choose_bind_ip(&ifaces, Some(Ipv4Addr::new(10, 5, 0, 2))).unwrap();
        assert_eq!(pick.name, "Wi-Fi");
        assert_eq!(pick.ip, Ipv4Addr::new(192, 168, 1, 20));
    }

    #[test]
    fn hyperv_vethernet_does_not_beat_wifi() {
        let ifaces = vec![
            cand("vEthernet (Default Switch)", [172, 22, 80, 1], 20),
            cand("Wi-Fi", [192, 168, 1, 42], 24),
        ];
        let pick = choose_bind_ip(&ifaces, Some(Ipv4Addr::new(172, 22, 80, 1))).unwrap();
        assert_eq!(pick.ip, Ipv4Addr::new(192, 168, 1, 42));
        assert_eq!(
            iface_role(
                "Hyper-V Virtual Ethernet Adapter",
                Ipv4Addr::new(172, 16, 0, 4)
            ),
            IfaceRole::Skip
        );
    }

    #[test]
    fn only_nordlynx_returns_no_bind_and_the_vpn_message() {
        let ifaces = vec![cand("NordLynx", [10, 5, 0, 2], 32)];
        assert!(choose_bind_ip(&ifaces, Some(Ipv4Addr::new(10, 5, 0, 2))).is_none());
        let tv = Ipv4Addr::new(10, 5, 0, 9);
        assert!(choose_bind_for_device(&ifaces, tv).is_none());
        assert!(NO_HOME_LAN_MESSAGE.contains("VPN"));
        assert!(NO_HOME_LAN_MESSAGE.contains("Tailscale"));
    }

    #[test]
    fn tv_subnet_match_wins_over_the_default_route() {
        let ifaces = vec![
            cand("Ethernet", [10, 0, 0, 8], 24),
            cand("Wi-Fi", [192, 168, 1, 20], 24),
            cand("NordLynx", [10, 5, 0, 2], 32),
        ];
        let route = choose_bind_ip(&ifaces, Some(Ipv4Addr::new(10, 0, 0, 8))).unwrap();
        assert_eq!(route.ip, Ipv4Addr::new(10, 0, 0, 8));
        let tv = Ipv4Addr::new(192, 168, 1, 56);
        let pick = choose_bind_for_device(&ifaces, tv).unwrap();
        assert_eq!(pick.name, "Wi-Fi");
        assert_eq!(pick.ip, Ipv4Addr::new(192, 168, 1, 20));
        let off_lan = Ipv4Addr::new(10, 5, 0, 9);
        assert!(choose_bind_for_device(&ifaces, off_lan).is_none());
    }

    #[test]
    fn bind_uses_default_route_when_it_is_lan() {
        let ifaces = vec![
            cand("Wi-Fi", [192, 168, 1, 20], 24),
            cand("Ethernet", [10, 0, 0, 8], 24),
        ];
        let pick = choose_bind_ip(&ifaces, Some(Ipv4Addr::new(10, 0, 0, 8))).unwrap();
        assert_eq!(pick.ip, Ipv4Addr::new(10, 0, 0, 8));
    }

    #[test]
    fn no_lan_means_no_bind() {
        let ifaces = vec![
            cand("Tailscale", [100, 64, 1, 2], 32),
            cand("ppp", [172, 16, 0, 2], 32),
        ];
        // 172.16 is RFC1918 but the name is not tailscale. 172.16.0.2 is preferred.
        let pick = choose_bind_ip(&ifaces, None).unwrap();
        assert_eq!(pick.ip, Ipv4Addr::new(172, 16, 0, 2));
        let only_ts = vec![cand("Tailscale", [100, 64, 1, 2], 32)];
        assert!(choose_bind_ip(&only_ts, None).is_none());
    }

    #[test]
    fn subnet_match_and_idle_timeout_wording() {
        let net = Ipv4Addr::new(192, 168, 1, 20);
        assert!(ipv4_in_subnet(Ipv4Addr::new(192, 168, 1, 55), net, 24));
        assert!(!ipv4_in_subnet(Ipv4Addr::new(192, 168, 2, 55), net, 24));
        assert!(is_mdns_idle_timeout("timed out waiting on a channel"));
        assert!(is_mdns_idle_timeout("Timeout"));
        assert!(!is_mdns_idle_timeout("channel is empty and closed"));
        assert!(NO_TV_ERROR.contains("No TV found on this network"));
        assert!(NO_TV_ERROR.contains("same Wi-Fi"));
        assert!(NO_TV_ERROR.contains("firewall"));
        assert!(NO_TV_ERROR.contains("client isolation"));
        assert!(!NO_TV_ERROR.to_ascii_lowercase().contains("living room"));
        let home = cand("Wi-Fi", [192, 168, 1, 20], 24);
        assert!(device_on_home_lan(
            Ipv4Addr::new(192, 168, 1, 40),
            &[home.clone()]
        ));
        assert!(!device_on_home_lan(
            Ipv4Addr::new(192, 168, 2, 40),
            &[home.clone()]
        ));
        assert!(!device_on_home_lan(
            Ipv4Addr::new(8, 8, 8, 8),
            &[home.clone()]
        ));
        let other = cand("Wi-Fi", [192, 168, 1, 21], 24);
        let fp_a = fingerprint_of(std::slice::from_ref(&home), Some(home.ip));
        let fp_b = fingerprint_of(std::slice::from_ref(&other), Some(other.ip));
        assert_ne!(fp_a, fp_b);
        assert_eq!(
            fp_a,
            fingerprint_of(std::slice::from_ref(&home), Some(home.ip))
        );
        assert!(parse_cast_host("192.168.1.40", None).is_ok());
        assert_eq!(
            parse_cast_host("  192.168.1.40  ", Some(8009)).unwrap().1,
            8009
        );
    }

    #[test]
    fn refuses_unsafe_bind_without_listening() {
        assert!(MediaHttp::start_lan(Ipv4Addr::UNSPECIFIED).is_err());
        assert!(MediaHttp::start_lan(Ipv4Addr::new(8, 8, 8, 8)).is_err());
        assert!(MediaHttp::start_lan(Ipv4Addr::new(100, 64, 0, 1)).is_err());
        assert!(MediaHttp::start_lan(Ipv4Addr::new(169, 254, 1, 1)).is_err());
        assert!((CAST_PORT_LO..=CAST_PORT_HI).all(|p| (47200..=47215).contains(&p)));
        assert_eq!(CAST_PORT_HI - CAST_PORT_LO, 15);
    }

    fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slidex-cast-{}-{}",
            std::process::id(),
            new_token()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path
    }

    fn jpeg_bytes() -> Vec<u8> {
        vec![0xff, 0xd8, 0xff, 0xd9]
    }

    fn png_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v
    }

    fn mp4_with(entries: &[&[u8; 4]]) -> Vec<u8> {
        fn box_of(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let size = (8 + payload.len()) as u32;
            let mut out = size.to_be_bytes().to_vec();
            out.extend_from_slice(typ);
            out.extend_from_slice(payload);
            out
        }
        let mut stsd_payload = vec![0, 0, 0, 0];
        stsd_payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for four in entries {
            // 4 size + 4 fourcc + 8 reserved = 16, matching the size field.
            let entry_size: u32 = 16;
            stsd_payload.extend_from_slice(&entry_size.to_be_bytes());
            stsd_payload.extend_from_slice(four.as_slice());
            stsd_payload.extend_from_slice(&[0u8; 8]);
        }
        let stsd = box_of(b"stsd", &stsd_payload);
        let stbl = box_of(b"stbl", &stsd);
        let minf = box_of(b"minf", &stbl);
        let mdia = box_of(b"mdia", &minf);
        let trak = box_of(b"trak", &mdia);
        let moov = box_of(b"moov", &trak);
        let mut ftyp = box_of(b"ftyp", b"isom");
        // A large mdat must be skipped, not scanned, and must not hide the moov codecs.
        let mdat = box_of(b"mdat", &[0x61, 0x76, 0x63, 0x31, 0, 0, 0, 0]);
        ftyp.extend_from_slice(&mdat);
        ftyp.extend_from_slice(&moov);
        ftyp
    }

    #[test]
    fn playlist_gate_accepts_jpeg_png_h264_and_rejects_the_rest() {
        let jpg = scratch("a.jpg", &jpeg_bytes());
        let png = scratch("b.png", &png_bytes());
        let mp4 = scratch("c.mp4", &mp4_with(&[b"avc1", b"mp4a"]));
        let hevc = scratch("d.mp4", &mp4_with(&[b"hvc1", b"mp4a"]));
        let hev1 = scratch("d2.mp4", &mp4_with(&[b"hev1", b"mp4a"]));
        let no_aac = scratch("e.mp4", &mp4_with(&[b"avc1"]));
        let webp = scratch("f.webp", b"RIFF");
        let mov = scratch("g.mov", &mp4_with(&[b"avc1", b"mp4a"]));
        assert!(classify_playlist_file(&jpg).is_ok());
        assert!(classify_playlist_file(&png).is_ok());
        assert!(classify_playlist_file(&mp4).is_ok());
        assert!(classify_playlist_file(&hevc).is_ok());
        assert!(classify_playlist_file(&hev1).is_ok());
        assert!(classify_playlist_file(&no_aac).is_err());
        assert!(classify_playlist_file(&webp).is_err());
        assert!(classify_playlist_file(&mov).is_err());
        assert!(mp4_is_h264_aac(&mp4).unwrap());
        assert!(!mp4_is_h264_aac(&hevc).unwrap());
        assert!(!mp4_is_h264_aac(&hev1).unwrap());
    }

    fn mp4_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(typ);
        out.extend_from_slice(payload);
        out
    }

    fn full_box(typ: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0];
        body.extend_from_slice(payload);
        mp4_box(typ, &body)
    }

    /// A video the TV-limit probe can measure. `level` `None` omits avcC/hvcC.
    /// `Some(0)` writes a level byte of 0. Bitrate `None` omits `btrt`.
    fn limited_mp4(
        fourcc: &[u8; 4],
        width: u32,
        height: u32,
        timescale: u32,
        sample_delta: u32,
        level: Option<u8>,
        hevc_idc: Option<u8>,
        bitrate: Option<u32>,
        pad: usize,
    ) -> Vec<u8> {
        let mut tkhd_rest = vec![0u8; 72];
        tkhd_rest.extend_from_slice(&(width << 16).to_be_bytes());
        tkhd_rest.extend_from_slice(&(height << 16).to_be_bytes());
        let tkhd = full_box(b"tkhd", 0, &tkhd_rest);
        let mut mdhd_rest = vec![0u8; 8];
        mdhd_rest.extend_from_slice(&timescale.to_be_bytes());
        mdhd_rest.extend_from_slice(&timescale.to_be_bytes());
        let mdhd = full_box(b"mdhd", 0, &mdhd_rest);
        let mut stts_rest = Vec::new();
        stts_rest.extend_from_slice(&1u32.to_be_bytes());
        stts_rest.extend_from_slice(&1u32.to_be_bytes());
        stts_rest.extend_from_slice(&sample_delta.to_be_bytes());
        let stts = full_box(b"stts", 0, &stts_rest);
        let mut visual = vec![0u8; 78];
        if let Some(idc) = hevc_idc {
            let mut hvcc = vec![0u8; 18];
            hvcc[0] = 1;
            hvcc[12] = idc;
            hvcc[17] = 0xf8;
            visual.extend_from_slice(&mp4_box(b"hvcC", &hvcc));
        } else if let Some(level_byte) = level {
            // Baseline profile, 8-bit, no SPS. Level is byte 3 (tenths).
            let avcc = [0x01u8, 0x42, 0x00, level_byte, 0xff, 0xe0];
            visual.extend_from_slice(&mp4_box(b"avcC", &avcc));
        }
        if let Some(avg) = bitrate {
            let mut btrt = vec![0u8; 4];
            btrt.extend_from_slice(&avg.to_be_bytes());
            btrt.extend_from_slice(&avg.to_be_bytes());
            visual.extend_from_slice(&mp4_box(b"btrt", &btrt));
        }
        let entry = mp4_box(fourcc, &visual);
        let mut stsd_payload = vec![0, 0, 0, 0];
        stsd_payload.extend_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend_from_slice(&entry);
        let stsd = mp4_box(b"stsd", &stsd_payload);
        let stbl = mp4_box(b"stbl", &[stsd, stts].concat());
        let minf = mp4_box(b"minf", &stbl);
        let mdia = mp4_box(b"mdia", &[mdhd, minf].concat());
        let video = mp4_box(b"trak", &[tkhd, mdia].concat());
        let mut audio_entry = Vec::new();
        audio_entry.extend_from_slice(&16u32.to_be_bytes());
        audio_entry.extend_from_slice(b"mp4a");
        audio_entry.extend_from_slice(&[0u8; 8]);
        let mut audio_stsd = vec![0, 0, 0, 0];
        audio_stsd.extend_from_slice(&1u32.to_be_bytes());
        audio_stsd.extend_from_slice(&audio_entry);
        let audio = mp4_box(
            b"trak",
            &mp4_box(
                b"mdia",
                &mp4_box(b"minf", &mp4_box(b"stbl", &mp4_box(b"stsd", &audio_stsd))),
            ),
        );
        let moov = mp4_box(b"moov", &[video, audio].concat());
        let mut file = mp4_box(b"ftyp", b"isom");
        file.extend_from_slice(&mp4_box(b"mdat", &vec![0u8; pad]));
        file.extend_from_slice(&moov);
        file
    }

    fn h264_clip(
        width: u32,
        height: u32,
        timescale: u32,
        sample_delta: u32,
        level: Option<u8>,
        bitrate: Option<u32>,
    ) -> Vec<u8> {
        limited_mp4(
            b"avc1",
            width,
            height,
            timescale,
            sample_delta,
            level,
            None,
            bitrate,
            8,
        )
    }

    #[test]
    fn level_5_1_passes_and_a_declared_level_is_trusted() {
        assert_eq!(CAST_MAX_VIDEO_LEVEL, 52);
        let exact = scratch(
            "level51.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(51), Some(8_000_000)),
        );
        let decision = decide_cast_video(&exact);
        assert!(decision.skip.is_none(), "{:?}", decision.skip);
        assert!(decision.derived.is_none(), "{:?}", decision.derived);
        // 2160×2160 @ 60 needs 5.2, but a declared 5.1 is not recomputed.
        let trusted = scratch(
            "declared51.mp4",
            &h264_clip(2160, 2160, 60, 1, Some(51), Some(8_000_000)),
        );
        let trusted = decide_cast_video(&trusted);
        assert!(trusted.skip.is_none(), "{:?}", trusted.skip);
        assert!(trusted.derived.is_none());
        let hevc = scratch(
            "hevc51.mp4",
            &limited_mp4(
                b"hvc1",
                1920,
                1080,
                30,
                1,
                None,
                Some(153),
                Some(8_000_000),
                8,
            ),
        );
        assert!(decide_cast_video(&hevc).skip.is_none());
        // 60000/1001 is under 60 fps and must not be skipped for frame rate.
        let film = scratch(
            "film.mp4",
            &h264_clip(1920, 1080, 60_000, 1001, Some(51), Some(8_000_000)),
        );
        assert!(decide_cast_video(&film).skip.is_none());
    }

    #[test]
    fn level_5_2_passes_and_a_higher_level_is_skipped() {
        // John's Google TV plays 2160×2160 at 60 fps, which is level 5.2.
        let square = scratch(
            "square52.mp4",
            &h264_clip(2160, 2160, 60, 1, Some(52), Some(8_000_000)),
        );
        let square = decide_cast_video(&square);
        assert!(square.skip.is_none(), "{:?}", square.skip);
        assert!(square.derived.is_none());
        let path = scratch(
            "level52.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(52), Some(8_000_000)),
        );
        let decision = decide_cast_video(&path);
        assert!(decision.skip.is_none(), "{:?}", decision.skip);
        assert!(decision.derived.is_none());
        let hevc = scratch(
            "hevc52.mp4",
            &limited_mp4(
                b"hvc1",
                1920,
                1080,
                30,
                1,
                None,
                Some(156),
                Some(8_000_000),
                8,
            ),
        );
        assert!(decide_cast_video(&hevc).skip.is_none());

        let five_three = scratch(
            "level53.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(53), Some(8_000_000)),
        );
        assert_eq!(
            decide_cast_video(&five_three).skip.as_deref(),
            Some("H.264 level 5.3 may not play on this TV")
        );
        let six = scratch(
            "level60.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(60), Some(8_000_000)),
        );
        assert_eq!(
            decide_cast_video(&six).skip.as_deref(),
            Some("H.264 level 6.0 may not play on this TV")
        );
        let six_one = scratch(
            "level61.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(61), Some(8_000_000)),
        );
        assert_eq!(
            decide_cast_video(&six_one).skip.as_deref(),
            Some("H.264 level 6.1 may not play on this TV")
        );
        // general_level_idc 180 is HEVC level 6.0.
        let hevc6 = scratch(
            "hevc60.mp4",
            &limited_mp4(
                b"hvc1",
                1920,
                1080,
                30,
                1,
                None,
                Some(180),
                Some(8_000_000),
                8,
            ),
        );
        assert_eq!(
            decide_cast_video(&hevc6).skip.as_deref(),
            Some("HEVC level 6.0 may not play on this TV")
        );
        let both = scratch(
            "fps-and-level.mp4",
            &h264_clip(1920, 1080, 90, 1, Some(60), Some(8_000_000)),
        );
        assert_eq!(
            decide_cast_video(&both).skip.as_deref(),
            Some("90 fps isn't supported by the TV. H.264 level 6.0 may not play on this TV")
        );
    }

    #[test]
    fn missing_or_zero_level_is_derived_from_macroblocks() {
        // 1920×1080 @ 30 → mb 120×68 = 8160, MBPS 244800 → level 4.0, and it plays.
        assert_eq!(derive_h264_level(1920, 1080, 30, 1), Some(40));
        // 2160×2160 @ 60 → mb 135×135 = 18225, MBPS 1_093_500 → level 5.2.
        assert_eq!(derive_h264_level(2160, 2160, 60, 1), Some(52));
        // A frame rate at or under 60 that still needs 5.1 passes once derived.
        assert_eq!(derive_h264_level(2000, 1536, 60, 1), Some(51));

        let zero = scratch(
            "level0.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(0), Some(8_000_000)),
        );
        let zero = decide_cast_video(&zero);
        assert_eq!(zero.derived.as_deref(), Some("4.0"));
        assert!(zero.skip.is_none(), "{:?}", zero.skip);

        let absent = scratch(
            "nolevel.mp4",
            &h264_clip(1920, 1080, 30, 1, None, Some(8_000_000)),
        );
        let absent = decide_cast_video(&absent);
        assert_eq!(absent.derived.as_deref(), Some("4.0"));
        assert!(absent.skip.is_none(), "{:?}", absent.skip);

        let square = scratch(
            "square.mp4",
            &h264_clip(2160, 2160, 60, 1, Some(0), Some(8_000_000)),
        );
        let square = decide_cast_video(&square);
        assert_eq!(square.derived.as_deref(), Some("5.2"));
        assert!(
            square.skip.is_none(),
            "a derived 5.2 plays: {:?}",
            square.skip
        );

        let passes = scratch(
            "derived51.mp4",
            &h264_clip(2000, 1536, 60, 1, Some(0), Some(8_000_000)),
        );
        let passes = decide_cast_video(&passes);
        assert_eq!(passes.derived.as_deref(), Some("5.1"));
        assert!(passes.skip.is_none(), "{:?}", passes.skip);

        let hevc_zero = scratch(
            "hevc0.mp4",
            &limited_mp4(
                b"hvc1",
                1920,
                1080,
                30,
                1,
                None,
                Some(0),
                Some(8_000_000),
                8,
            ),
        );
        let hevc_zero = decide_cast_video(&hevc_zero);
        assert_eq!(hevc_zero.derived.as_deref(), Some("4.0"));
        assert!(hevc_zero.skip.is_none(), "{:?}", hevc_zero.skip);
    }

    #[test]
    fn unsupported_limits_are_named_in_one_sentence() {
        let path = scratch(
            "too-big.mp4",
            &h264_clip(2560, 2560, 90, 1, Some(51), Some(245_000_000)),
        );
        assert_eq!(
            decide_cast_video(&path).skip.as_deref(),
            Some("90 fps, 2560x2560 and 245 Mbit/s aren't supported by the TV")
        );
        let square = scratch(
            "square-ok.mp4",
            &h264_clip(2160, 2160, 30, 1, Some(51), Some(8_000_000)),
        );
        assert!(decide_cast_video(&square).skip.is_none());
        let hid = scratch(
            "hid.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(51), Some(20_400_000)),
        );
        assert_eq!(
            decide_cast_video(&hid).skip.as_deref(),
            Some("20.4 Mbit/s isn't supported by the TV")
        );
        let sized = scratch(
            "sized.mp4",
            &limited_mp4(b"avc1", 1920, 1080, 30, 1, Some(51), None, None, 3_000_000),
        );
        let len = std::fs::metadata(&sized).unwrap().len();
        let rate = bitrate_from_file(len, 30, 30).unwrap();
        assert!(rate > 20_000_000, "{rate}");
        assert_eq!(
            decide_cast_video(&sized).skip.as_deref(),
            Some(format!(
                "{} isn't supported by the TV",
                format_mbit(rate, 20_000_000)
            ))
            .as_deref()
        );
        let hev = scratch(
            "hev1.mp4",
            &limited_mp4(
                b"hev1",
                1920,
                1080,
                30,
                1,
                None,
                Some(153),
                Some(8_000_000),
                8,
            ),
        );
        assert_eq!(
            decide_cast_video(&hev).skip.as_deref(),
            Some("HEVC (hev1) isn't supported by the TV")
        );
        // A stub sample entry has no size, rate, or level, so it still loads.
        let stub = scratch("stub.mp4", &mp4_with(&[b"avc1", b"mp4a"]));
        let stub = decide_cast_video(&stub);
        assert!(stub.skip.is_none(), "{:?}", stub.skip);
        assert!(stub.derived.is_none());
    }

    #[test]
    fn auto_advance_skips_an_unsupported_video_and_logs_it() {
        let log = LogDir::new();
        let photo = scratch("one.jpg", &jpeg_bytes());
        let bad = scratch(
            "skip-me.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(60), Some(8_000_000)),
        );
        let next = scratch("three.jpg", &jpeg_bytes());
        let derived = scratch(
            "derived.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(0), Some(8_000_000)),
        );
        let http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        http.set_playlist(&[photo, bad, next.clone(), derived])
            .unwrap();
        let started = Instant::now();
        let (succ, skipped) = http.next_castable(1);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a skipped video must not wait out the load deadline: {:?}",
            started.elapsed()
        );
        let succ = succ.expect("the photo after the bad video");
        assert_eq!(succ.item, 3);
        assert_eq!(succ.media_type, "photo");
        assert_eq!(
            skipped.as_deref(),
            Some("H.264 level 6.0 may not play on this TV")
        );
        let written = log.text();
        assert!(written.contains("video_skipped"), "{written}");
        assert!(written.contains("item=2"), "{written}");
        assert!(
            written.contains("reasons=H.264 level 6.0 may not play on this TV"),
            "{written}"
        );
        let (play, none) = http.next_castable(3);
        let play = play.expect("derived 4.0 video");
        assert_eq!(play.item, 4);
        assert_eq!(play.media_type, "video");
        assert!(none.is_none());
        let written = log.text();
        assert!(written.contains("level_derived=4.0"), "{written}");
    }

    fn drive_load(
        name: &str,
        bytes: &[u8],
    ) -> (LogDir, FakeTv, Result<(), String>, CastLiveStatus) {
        let log = LogDir::new();
        let path = scratch(name, bytes);
        let mut http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        http.set_playlist(std::slice::from_ref(&path)).unwrap();
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let status = Mutex::new(CastLiveStatus {
            connected: true,
            player_state: "idle".into(),
            remote_event: None,
            error: None,
            media_kind: String::new(),
            http_port: http.port,
            bind_ip: http.bind_ip.to_string(),
            item_index: 0,
        });
        let (_remote, mut flags) = session_after_load("still");
        flags.media_kind.clear();
        flags.media_session = None;
        flags.player_state = "idle".into();
        flags.item_index = 0;
        let mut running = true;
        let (tx, rx) = mpsc::channel();
        let kept = handle_cmd(
            CastCmd::Load {
                path,
                autoplay: true,
                origin: CastOrigin::User,
                duration_ms: None,
                reply: tx,
            },
            &mut http,
            &link,
            &mut flags,
            &status,
            &mut running,
        );
        assert!(kept);
        let result = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let live = status.lock().unwrap().clone();
        // Keep the link up until the reply is in hand. Drop order is the
        // return tuple, then these locals.
        let _ = link;
        let _ = http;
        (log, fake, result, live)
    }

    #[test]
    fn level_5_2_load_is_sent() {
        let (log, fake, result, live) = drive_load(
            "level52.mp4",
            &h264_clip(2160, 2160, 60, 1, Some(52), Some(8_000_000)),
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(live.error.is_none(), "{:?}", live.error);
        assert_eq!(live.media_kind, "video");
        let text = fake.trace_text();
        assert!(
            text.lines().any(|line| line.starts_with("load ")),
            "level 5.2 is loaded: {text}"
        );
        let written = log.text();
        assert!(!written.contains("video_skipped"), "{written}");
        assert!(!written.contains("level_derived="), "{written}");
    }

    #[test]
    fn level_above_5_2_load_sends_no_load_and_logs_video_skipped() {
        let (log, fake, result, live) = drive_load(
            "no-load.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(53), Some(8_000_000)),
        );
        let err = result.expect_err("a level above 5.2 is not loaded");
        assert_eq!(err, "H.264 level 5.3 may not play on this TV");
        assert!(live.connected);
        assert_eq!(live.error.as_deref(), Some(err.as_str()));
        let text = fake.trace_text();
        assert!(
            !text.lines().any(|line| line.starts_with("load ")),
            "no LOAD is sent: {text}"
        );
        let written = log.text();
        assert!(written.contains("video_skipped"), "{written}");
        assert!(
            written.contains("item=1 reasons=H.264 level 5.3 may not play on this TV"),
            "{written}"
        );
        assert!(!written.contains("level_derived="), "{written}");
    }

    #[test]
    fn derived_level_4_0_still_loads_and_logs_level_derived() {
        let (log, fake, result, live) = drive_load(
            "derived-load.mp4",
            &h264_clip(1920, 1080, 30, 1, Some(0), Some(8_000_000)),
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(live.error.is_none(), "{:?}", live.error);
        assert_eq!(live.media_kind, "video");
        let text = fake.trace_text();
        assert!(
            text.lines().any(|line| line.starts_with("load ")),
            "a derived 4.0 file is loaded: {text}"
        );
        let written = log.text();
        assert!(written.contains("level_derived=4.0"), "{written}");
        assert!(written.contains("item=1"), "{written}");
        assert!(!written.contains("video_skipped"), "{written}");
    }

    #[test]
    fn token_allowlist_serves_only_playlist_files_and_blocks_traversal() {
        let jpg = scratch("slide.jpg", &jpeg_bytes());
        let other = scratch("secret.jpg", &jpeg_bytes());
        let png = scratch("two.png", &png_bytes());
        let http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        assert_eq!(http.bind_ip, Ipv4Addr::LOCALHOST);
        assert_ne!(http.bind_ip, Ipv4Addr::UNSPECIFIED);
        let update = http
            .set_playlist(&[jpg.clone(), png.clone(), other.with_file_name("nope.webp")])
            .unwrap();
        assert_eq!(update.registered, 2);
        assert!(!update.skipped.is_empty());

        let (url_jpg, _) = http.media_url(&jpg).unwrap();
        let (url_png, _) = http.media_url(&png).unwrap();
        assert!(http.media_url(&other).is_err());
        let token = {
            let book = http.book.lock().unwrap();
            assert_eq!(book.token.len(), 32);
            assert!(!url_jpg.contains(&jpg.display().to_string()));
            assert!(url_jpg.contains(&book.token));
            book.token.clone()
        };
        assert_ne!(
            url_jpg.split('/').last().unwrap(),
            url_png.split('/').last().unwrap()
        );

        let (status, body) = http_get(http.bind_ip, http.port, &url_path(&url_jpg));
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body, jpeg_bytes());

        let (status, _) = http_get(http.bind_ip, http.port, "/");
        assert_eq!(status, 404);
        let (status, _) = http_get(http.bind_ip, http.port, "/m/");
        assert_eq!(status, 404);
        let (status, _) = http_get(http.bind_ip, http.port, &format!("/m/{token}"));
        assert_eq!(status, 404);
        let (status, _) = http_get(
            http.bind_ip,
            http.port,
            &format!("/m/{token}/{}", "deadbeef".repeat(4)),
        );
        assert_eq!(status, 404);
        let (status, _) = http_get(
            http.bind_ip,
            http.port,
            &format!("/m/{token}/../../etc/passwd"),
        );
        assert_eq!(status, 404);
        let (status, _) = http_get(
            http.bind_ip,
            http.port,
            &format!("/m/{token}/%2e%2e/secret.jpg"),
        );
        assert_eq!(status, 404);
        let (status, _) = http_get(
            http.bind_ip,
            http.port,
            &format!("/m/not-the-token/{}", url_jpg.split('/').last().unwrap()),
        );
        assert_eq!(status, 404);

        // Path is not a URL key. Using the filename must not serve the file.
        let (status, _) = http_get(http.bind_ip, http.port, &format!("/m/{token}/slide.jpg"));
        assert_eq!(status, 404);

        let (status, body) = http_get(
            http.bind_ip,
            http.port,
            &format!("{}?x=1", url_path(&url_png)),
        );
        assert_eq!(status, 200);
        assert!(body.starts_with(b"\x89PNG"));

        // Range for video-style clients.
        let (status, hdr_body) =
            http_get_range(http.bind_ip, http.port, &url_path(&url_jpg), "bytes=1-2");
        assert_eq!(status, 206, "{hdr_body:?}");
        assert_eq!(hdr_body, vec![0xd8, 0xff]);

        // Revoke on disconnect: a new token and an empty book.
        {
            let mut book = http.book.lock().unwrap();
            book.revoke();
            assert!(book.files.is_empty());
            assert_ne!(book.token, token);
        }
        let (status, _) = http_get(http.bind_ip, http.port, &url_path(&url_jpg));
        assert_eq!(status, 404);

        let port = http.port;
        drop(http);
        let refused = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(400),
        );
        assert!(
            refused.is_err(),
            "media server must close when casting stops"
        );
    }

    #[test]
    fn resolve_helper_rejects_listing_and_foreign_keys() {
        let mut book = AllowBook::new();
        let path = scratch("only.jpg", &jpeg_bytes());
        let update = replace_playlist(&mut book, &[path]);
        assert_eq!(update.registered, 1);
        let key = book.files.keys().next().unwrap().clone();
        let token = book.token.clone();
        assert!(resolve_media_url(&format!("/m/{token}/{key}"), &book).is_ok());
        assert!(resolve_media_url("/", &book).is_err());
        assert!(resolve_media_url(&format!("/m/{token}/{key}/extra"), &book).is_err());
        assert!(resolve_media_url(&format!("/m/{token}/../{key}"), &book).is_err());
        let mut other = AllowBook::new();
        assert!(resolve_media_url(&format!("/m/{}/{}", other.token, key), &book).is_err());
        other.revoke();
    }

    #[test]
    fn manual_ip_rejects_public_and_accepts_lan() {
        assert!(parse_cast_host("192.168.1.55", None).is_ok());
        assert!(parse_cast_host("8.8.8.8", Some(8009)).is_err());
        assert!(parse_cast_host("100.64.0.2", None).is_err());
        assert!(parse_cast_host("not-a-host", None).is_err());
        assert!(parse_cast_host("192.168.1.55", Some(0)).is_err());
    }

    #[test]
    fn cast_notice_covers_public_private_and_unknown_for_both_scenarios() {
        assert_ne!(PUBLIC_WIFI_MESSAGE, NO_TV_ERROR);
        assert_ne!(PUBLIC_WIFI_MESSAGE, MEDIA_LOAD_TIMEOUT_MESSAGE);
        assert_ne!(NO_TV_ERROR, MEDIA_LOAD_TIMEOUT_MESSAGE);

        assert_eq!(
            cast_notice(CastNotice::NoTv, NetCategory::Public),
            PUBLIC_WIFI_MESSAGE
        );
        assert_eq!(
            cast_notice(CastNotice::MediaLoadTimeout, NetCategory::Public),
            PUBLIC_WIFI_MESSAGE
        );
        assert_eq!(
            cast_notice(CastNotice::NoTv, NetCategory::Private),
            NO_TV_ERROR
        );
        assert_eq!(
            cast_notice(CastNotice::NoTv, NetCategory::Unknown),
            NO_TV_ERROR
        );
        assert_eq!(
            cast_notice(CastNotice::MediaLoadTimeout, NetCategory::Private),
            MEDIA_LOAD_TIMEOUT_MESSAGE
        );
        assert_eq!(
            cast_notice(CastNotice::MediaLoadTimeout, NetCategory::Unknown),
            MEDIA_LOAD_TIMEOUT_MESSAGE
        );

        assert_eq!(parse_net_category("Public"), NetCategory::Public);
        assert_eq!(parse_net_category("private"), NetCategory::Private);
        assert_eq!(
            parse_net_category("DomainAuthenticated"),
            NetCategory::Private
        );
        assert_eq!(parse_net_category(""), NetCategory::Unknown);
    }

    fn fetch_load_url(http_ip: Ipv4Addr, http_port: u16, frame_url: &str, method: &str, range: Option<&str>) -> HttpResp {
        let path = url_path(frame_url);
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {http_ip}:{http_port}\r\nConnection: close\r\n");
        if let Some(range) = range {
            req.push_str(&format!("Range: {range}\r\n"));
        }
        req.push_str("\r\n");
        http_exchange_full(http_ip, http_port, &req)
    }

    #[test]
    fn load_frame_url_is_served_by_the_media_server() {
        let photo = scratch("vacation-photo.jpg", &jpeg_bytes());
        let video = scratch("clip.mp4", &mp4_with(&[b"avc1", b"mp4a"]));
        let http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        let update = http.set_playlist(&[photo.clone(), video.clone()]).unwrap();
        assert_eq!(update.registered, 2);
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();

        let (photo_url, photo_type) = http.media_url(&photo).unwrap();
        assert!(photo_type.starts_with("image/"), "{photo_type}");
        link.load_url(&photo_url, &photo_type, true, CastOrigin::User, 1, "photo")
            .unwrap();
        let photo_frame = fake.content_ids().last().cloned().unwrap_or_default();
        assert_eq!(photo_frame, photo_url, "LOAD contentId must be the registered media URL");
        assert!(!photo_frame.contains("vacation-photo"), "{photo_frame}");
        let got = fetch_load_url(http.bind_ip, http.port, &photo_frame, "GET", None);
        assert_eq!(got.status, 200, "photo GET headers:\n{}", got.headers);
        assert!(
            got.header("content-type").starts_with("image/"),
            "{}",
            got.header("content-type")
        );
        assert_eq!(got.body, jpeg_bytes());
        let head = fetch_load_url(http.bind_ip, http.port, &photo_frame, "HEAD", None);
        assert_eq!(head.status, 200, "photo HEAD headers:\n{}", head.headers);
        assert!(head.header("content-type").starts_with("image/"));
        assert!(head.body.is_empty(), "HEAD must not carry a body");

        let (video_url, video_type) = http.media_url(&video).unwrap();
        assert_eq!(video_type, "video/mp4");
        link.load_url(&video_url, &video_type, true, CastOrigin::User, 2, "video")
            .unwrap();
        let video_frame = fake.content_ids().last().cloned().unwrap_or_default();
        assert_eq!(video_frame, video_url, "LOAD contentId must be the registered media URL");
        assert_ne!(video_frame, photo_frame);
        let got = fetch_load_url(http.bind_ip, http.port, &video_frame, "GET", None);
        assert_eq!(got.status, 200, "video GET headers:\n{}", got.headers);
        assert_eq!(got.header("content-type"), "video/mp4");
        assert!(got.header("accept-ranges").eq_ignore_ascii_case("bytes"));
        assert!(!got.body.is_empty());
        let head = fetch_load_url(http.bind_ip, http.port, &video_frame, "HEAD", None);
        assert_eq!(head.status, 200, "video HEAD headers:\n{}", head.headers);
        assert_eq!(head.header("content-type"), "video/mp4");
        assert!(head.body.is_empty());
        let range = fetch_load_url(
            http.bind_ip,
            http.port,
            &video_frame,
            "GET",
            Some("bytes=0-3"),
        );
        assert_eq!(range.status, 206, "video range headers:\n{}", range.headers);
        assert_eq!(range.header("content-type"), "video/mp4");
        assert!(range.header("accept-ranges").eq_ignore_ascii_case("bytes"));
        assert!(range.header("content-range").starts_with("bytes 0-3/"));
        assert_eq!(range.body.len(), 4);
    }

    #[test]
    fn panel_message_keeps_unreachable_for_connect_failures_only() {
        let fetch = cast_panel_message(CastPanelCause::MediaCommandTimeout);
        let load = cast_panel_message(CastPanelCause::LoadError);
        let down = cast_panel_message(CastPanelCause::ConnectFailed);
        assert_eq!(fetch, MEDIA_FETCH_MESSAGE);
        assert_eq!(load, MEDIA_FETCH_MESSAGE);
        assert_eq!(down, link_down_message());
        assert_ne!(fetch, down);
        assert!(down.contains("TV unreachable"));
        for msg in [fetch.as_str(), load.as_str()] {
            let lower = msg.to_ascii_lowercase();
            assert!(!lower.contains("unreachable"), "{msg}");
            assert!(!msg.contains("restart SlideX"), "{msg}");
            assert!(!lower.contains("not connected"), "{msg}");
            assert!(!lower.contains("unsupported"), "{msg}");
            assert!(!msg.contains("skipped"), "{msg}");
        }
        assert_eq!(load_reject_user("LOAD_FAILED"), MEDIA_FETCH_MESSAGE);
        assert_eq!(load_reject_user("INVALID_REQUEST"), MEDIA_FETCH_MESSAGE);
        assert_eq!(
            load_reject_user("INVALID_PLAYER_STATE"),
            MEDIA_FETCH_MESSAGE
        );
        assert_eq!(
            load_reject_user("LOAD_CANCELLED"),
            "The TV cancelled this slide."
        );
        assert_eq!(media_load_notice(), MEDIA_FETCH_MESSAGE);
        assert!(link_down_message().contains("TV unreachable"));
    }

    #[test]
    fn media_server_logs_the_bind_and_one_redacted_request_line() {
        let log = LogDir::new();
        let jpg = scratch("secret-vacation-photo.jpg", &jpeg_bytes());
        let http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        assert_eq!(http.bind_ip, Ipv4Addr::LOCALHOST);
        assert_ne!(http.bind_ip, Ipv4Addr::UNSPECIFIED);
        assert!(MediaHttp::start_lan(Ipv4Addr::UNSPECIFIED).is_err());
        http.set_playlist(std::slice::from_ref(&jpg)).unwrap();
        let (url, _) = http.media_url(&jpg).unwrap();
        let token = {
            let book = http.book.lock().unwrap();
            book.token.clone()
        };
        let key = url.rsplit('/').next().unwrap().to_string();
        let host = media_url_host(&url).unwrap();
        assert_eq!(host, format!("127.0.0.1:{}", http.port));
        assert!(url.starts_with(&format!("http://{}:{}/", http.bind_ip, http.port)));
        assert!(!url.contains("0.0.0.0"));
        let (status, body) = http_get(http.bind_ip, http.port, &url_path(&url));
        assert_eq!(status, 200);
        assert_eq!(body, jpeg_bytes());
        let (missing, _) = http_get(
            http.bind_ip,
            http.port,
            "/m/not-a-token/secret-vacation-photo.jpg",
        );
        assert_eq!(missing, 404);
        let written = log.text();
        let listen = written
            .lines()
            .find(|l| l.contains(" media_listen "))
            .unwrap_or_else(|| panic!("missing media_listen in {written}"));
        assert!(
            listen.contains(&format!("addr=127.0.0.1:{}", http.port)),
            "{listen}"
        );
        assert!(
            listen.contains(&format!("pid={}", std::process::id())),
            "{listen}"
        );
        assert!(!listen.contains('/') && !listen.contains('\\'), "{listen}");
        let reqs: Vec<_> = written
            .lines()
            .filter(|l| l.contains(" media_req "))
            .collect();
        assert_eq!(reqs.len(), 2, "{written}");
        let ok = reqs.iter().find(|l| l.contains("status=200")).unwrap();
        assert!(ok.contains("from=127.0.0.1"), "{ok}");
        assert!(ok.contains("method=GET"), "{ok}");
        assert!(ok.contains(&format!("path=/m/{key}?t=***")), "{ok}");
        assert!(ok.contains("ctype=image/jpeg"), "{ok}");
        assert!(ok.contains(&format!("bytes={}", body.len())), "{ok}");
        assert!(!ok.contains(&token), "{ok}");
        assert!(!ok.contains("secret"), "{ok}");
        assert!(!ok.contains("vacation"), "{ok}");
        assert!(!ok.contains('\\'), "{ok}");
        let denied = reqs.iter().find(|l| l.contains("status=404")).unwrap();
        assert!(denied.contains("path=redacted"), "{denied}");
        assert!(denied.contains("ctype=none"), "{denied}");
        assert!(denied.contains("bytes=0"), "{denied}");
        assert!(!denied.contains("secret"), "{denied}");
        assert!(!denied.contains("jpg"), "{denied}");
        assert!(!denied.contains("not-a-token"), "{denied}");
        assert!(!written.contains("secret-vacation"), "{written}");
        assert!(!written.contains(&token), "{written}");
    }

    fn big_jpeg(len: usize) -> Vec<u8> {
        let mut v = vec![0xff, 0xd8, 0xff];
        v.resize(len, 0x11);
        v
    }

    fn hold_transfer(ip: Ipv4Addr, port: u16, path: &str) -> TcpStream {
        let mut slow =
            TcpStream::connect_timeout(&SocketAddr::from((ip, port)), Duration::from_secs(2))
                .unwrap();
        shrink_recv_buffer(&slow);
        slow.set_read_timeout(Some(Duration::from_millis(200))).ok();
        slow.set_nodelay(true).ok();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        slow.write_all(req.as_bytes()).unwrap();
        slow
    }

    /// A small receive window keeps a large response blocked in `write`, instead of
    /// sitting entirely in the kernel buffer.
    fn shrink_recv_buffer(sock: &TcpStream) {
        let sz: i32 = 1024;
        #[cfg(unix)]
        unsafe {
            use std::os::fd::AsRawFd;
            extern "C" {
                fn setsockopt(
                    sockfd: i32,
                    level: i32,
                    optname: i32,
                    optval: *const u8,
                    optlen: u32,
                ) -> i32;
            }
            // Linux SOL_SOCKET = 1, SO_RCVBUF = 8.
            let _ = setsockopt(sock.as_raw_fd(), 1, 8, &sz as *const i32 as *const u8, 4);
        }
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawSocket;
            extern "system" {
                fn setsockopt(
                    s: usize,
                    level: i32,
                    optname: i32,
                    optval: *const u8,
                    optlen: i32,
                ) -> i32;
            }
            // Windows SOL_SOCKET = 0xffff, SO_RCVBUF = 0x1002.
            let _ = setsockopt(
                sock.as_raw_socket() as usize,
                0xffff,
                0x1002,
                &sz as *const i32 as *const u8,
                4,
            );
        }
    }

    #[test]
    fn slow_reader_does_not_block_the_next_request() {
        let big_path = scratch("big.jpg", &big_jpeg(2 * 1024 * 1024));
        let small_path = scratch("small.jpg", &jpeg_bytes());
        let http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        http.set_playlist(&[big_path.clone(), small_path.clone()])
            .unwrap();
        let (url_big, _) = http.media_url(&big_path).unwrap();
        let (url_small, _) = http.media_url(&small_path).unwrap();
        let mut slow = hold_transfer(http.bind_ip, http.port, &url_path(&url_big));
        thread::sleep(Duration::from_millis(250));

        let started = Instant::now();
        let (status, body) = http_get(http.bind_ip, http.port, &url_path(&url_small));
        let elapsed = started.elapsed();
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body, jpeg_bytes());
        assert!(
            elapsed < Duration::from_secs(1),
            "second request took {elapsed:?}"
        );

        slow.set_read_timeout(Some(Duration::from_millis(100))).ok();
        let mut got = 0usize;
        let mut buf = [0u8; 8192];
        if let Ok(n) = slow.read(&mut buf) {
            got += n;
        }
        assert!(
            got < 256 * 1024,
            "slow client should still be mid-transfer, read {got} bytes"
        );
    }

    #[test]
    fn shutdown_during_transfer_returns_within_one_second_and_closes_the_port() {
        let path = scratch("hold.jpg", &big_jpeg(2 * 1024 * 1024));
        let mut http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        http.set_playlist(&[path.clone()]).unwrap();
        let (url, _) = http.media_url(&path).unwrap();
        let port = http.port;
        let ip = http.bind_ip;
        let _slow = hold_transfer(ip, port, &url_path(&url));
        thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        http.shutdown();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "shutdown took {elapsed:?}"
        );
        let refused =
            TcpStream::connect_timeout(&SocketAddr::from((ip, port)), Duration::from_millis(400));
        assert!(refused.is_err(), "port must be closed after shutdown");
    }

    #[test]
    fn revoked_token_is_404_including_mid_transfer() {
        let path = scratch("rev.jpg", &big_jpeg(2 * 1024 * 1024));
        let http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        http.set_playlist(&[path.clone()]).unwrap();
        let (url, _) = http.media_url(&path).unwrap();
        let req_path = url_path(&url);
        let mut slow = hold_transfer(http.bind_ip, http.port, &req_path);
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let started = Instant::now();
        while got.len() < 1024 && started.elapsed() < Duration::from_secs(2) {
            match slow.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {
                    continue
                }
                Err(_) => break,
            }
        }
        assert!(
            !got.is_empty() && got.len() < 2 * 1024 * 1024,
            "expected an in-progress body, got {} bytes",
            got.len()
        );
        {
            let mut book = http.book.lock().unwrap();
            book.revoke();
        }
        let before = got.len();
        let drain_until = Instant::now() + Duration::from_millis(800);
        while Instant::now() < drain_until && got.len() < 2 * 1024 * 1024 {
            match slow.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        assert!(
            got.len() < 512 * 1024,
            "revoked transfer must stop, had {before} bytes then {}",
            got.len()
        );
        let (status, _) = http_get(http.bind_ip, http.port, &req_path);
        assert_eq!(status, 404);
    }

    #[test]
    fn stalled_client_is_closed_after_idle_and_releases_its_slot() {
        assert_eq!(HTTP_IDLE, Duration::from_secs(30));
        let idle = Duration::from_millis(800);
        let big_path = scratch("idle.jpg", &big_jpeg(4 * 1024 * 1024));
        let small_path = scratch("idle-small.jpg", &jpeg_bytes());
        let http =
            MediaHttp::bind_with_idle(Ipv4Addr::LOCALHOST, std::iter::once(0), idle).unwrap();
        http.set_playlist(&[big_path.clone(), small_path.clone()])
            .unwrap();
        let (url_big, _) = http.media_url(&big_path).unwrap();
        let (url_small, _) = http.media_url(&small_path).unwrap();
        let _held: Vec<_> = (0..HTTP_CONN_CAP)
            .map(|_| hold_transfer(http.bind_ip, http.port, &url_path(&url_big)))
            .collect();
        thread::sleep(Duration::from_millis(200));
        let (busy, _) = http_get(http.bind_ip, http.port, &url_path(&url_small));
        assert_eq!(
            busy, 503,
            "eight stalled transfers should hold every connection slot"
        );

        thread::sleep(idle + Duration::from_millis(400));
        let (status, body) = http_get(http.bind_ip, http.port, &url_path(&url_small));
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body, jpeg_bytes());
    }

    fn fw_line(
        name: &str,
        program: &str,
        profile: &str,
        remote: &str,
        protocol: &str,
        ports: &str,
    ) -> String {
        serde_json::json!({
            "name": name,
            "enabled": true,
            "direction": "Inbound",
            "action": "Allow",
            "profile": profile,
            "program": program,
            "remote": remote,
            "protocol": protocol,
            "localport": ports
        })
        .to_string()
    }

    #[test]
    fn firewall_query_uses_system32_powershell_without_path() {
        let exe = powershell_exe_under_windows_dir(r"C:\Windows");
        assert_eq!(
            exe,
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        );
        assert!(exe.contains(r"System32\WindowsPowerShell\v1.0\powershell.exe"));
        assert_ne!(exe.to_ascii_lowercase(), "powershell.exe");
        let script = Path::new(r"C:\SlideX\cast-fw-check.ps1");
        let program = r"C:\O'Brien\slideshowpro.exe";
        let args = firewall_powershell_file_args(script, program);
        assert_eq!(
            &args[..6],
            [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\SlideX\cast-fw-check.ps1"
            ]
        );
        assert_eq!(args[6], "-ExePath");
        assert_eq!(args[7], program);
        assert_eq!(
            quote_powershell_arg(program),
            "\"C:\\O'Brien\\slideshowpro.exe\""
        );
        assert!(!args.iter().any(|arg| arg == "-Command"));
        assert!(!args
            .iter()
            .any(|arg| arg.contains("'$") || arg.contains(&format!("'{program}'"))));
        let joined = args.join(" ");
        assert!(!joined.contains("-Command"));
        assert!(joined.contains("-ExecutionPolicy Bypass -File"));
        assert_eq!(parse_firewall_script_output("status=match").state, "added");
        assert_eq!(
            parse_firewall_script_output("status=partial").state,
            "partial"
        );
        assert_eq!(
            parse_firewall_script_output("status=nomatch").state,
            "missing"
        );
        let silent = parse_firewall_script_output("status=skipped_silent");
        let nomatch = parse_firewall_script_output("status=nomatch");
        assert_eq!(silent.state, "missing");
        assert_eq!(silent.detail, nomatch.detail);
        assert_eq!(
            parse_firewall_script_output("query failed: denied").state,
            "unknown"
        );
        assert_eq!(
            parse_firewall_script_output("query failed: denied").detail,
            "Could not query the Windows firewall."
        );
        let missing = parse_firewall_script_output(FW_SCRIPT_MISSING_QUERY);
        assert_eq!(missing.state, "unknown");
        assert_eq!(missing.detail, FW_SCRIPT_MISSING);
        assert!(missing.detail.contains("cast-fw-check.ps1"));
        assert!(!missing.detail.contains('\\'));
        assert!(!missing.detail.contains("device"));
        let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("windows")
            .join("cast-fw-check.ps1");
        let body = std::fs::read_to_string(&bundled).unwrap();
        assert!(body.contains("Get-NetFirewallRule"));
        assert!(body.contains("Get-NetFirewallApplicationFilter"));
        assert!(body.contains("Get-NetFirewallAddressFilter"));
        assert!(body.contains("Get-NetFirewallPortFilter"));
        assert!(body.contains("status=match"));
    }

    #[test]
    fn firewall_parser_requires_private_profile_and_ports() {
        let exe = r"C:\Users\qa\AppData\Local\SlideShowX\slideshowpro.exe";
        let media = fw_line(
            FW_MEDIA_RULE,
            exe,
            "Private",
            "LocalSubnet",
            "TCP",
            "47200-47215",
        );
        let mdns = fw_line(FW_MDNS_RULE, exe, "Private", "LocalSubnet", "UDP", "5353");
        let ok = parse_firewall_report(&format!("{media}\n{mdns}"), exe);
        assert_eq!(ok.state, "added");
        let prefixed = fw_line(
            FW_MEDIA_RULE,
            r"\\?\C:\Users\qa\AppData\Local\SlideShowX\slideshowpro.exe",
            "Private",
            "LocalSubnet",
            "TCP",
            "47200-47215",
        );
        assert_eq!(
            parse_firewall_report(&format!("{prefixed}\n{mdns}"), exe).state,
            "added"
        );

        let wrong = fw_line(
            FW_MEDIA_RULE,
            r"C:\Other\slideshowpro.exe",
            "Private",
            "LocalSubnet",
            "TCP",
            "47200-47215",
        );
        assert_ne!(
            parse_firewall_report(&format!("{wrong}\n{mdns}"), exe).state,
            "added"
        );
        let public_only = fw_line(
            FW_MEDIA_RULE,
            exe,
            "Public",
            "LocalSubnet",
            "TCP",
            "47200-47215",
        );
        assert_ne!(
            parse_firewall_report(&format!("{public_only}\n{mdns}"), exe).state,
            "added"
        );
        let no_subnet = fw_line(FW_MEDIA_RULE, exe, "Private", "Any", "TCP", "47200-47215");
        assert_ne!(
            parse_firewall_report(&format!("{no_subnet}\n{mdns}"), exe).state,
            "added"
        );
        assert_eq!(parse_firewall_report("", exe).state, "missing");
        assert_eq!(
            parse_firewall_report("query failed: powershell", exe).state,
            "unknown"
        );
    }

    fn url_path(url: &str) -> String {
        let rest = url.split("://").nth(1).unwrap();
        let path = rest.split_once('/').map(|(_, p)| p).unwrap_or("");
        format!("/{path}")
    }

    fn http_get(ip: Ipv4Addr, port: u16, path: &str) -> (u16, Vec<u8>) {
        http_exchange(
            ip,
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        )
    }

    fn http_get_range(ip: Ipv4Addr, port: u16, path: &str, range: &str) -> (u16, Vec<u8>) {
        http_exchange(
            ip,
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nRange: {range}\r\nConnection: close\r\n\r\n"),
        )
    }

    struct HttpResp {
        status: u16,
        headers: String,
        body: Vec<u8>,
    }

    impl HttpResp {
        fn header(&self, name: &str) -> String {
            for line in self.headers.split("\r\n") {
                if let Some((n, v)) = line.split_once(':') {
                    if n.eq_ignore_ascii_case(name) {
                        return v.trim().to_string();
                    }
                }
            }
            String::new()
        }
    }

    fn http_exchange(ip: Ipv4Addr, port: u16, req: &str) -> (u16, Vec<u8>) {
        let resp = http_exchange_full(ip, port, req);
        (resp.status, resp.body)
    }

    fn http_exchange_full(ip: Ipv4Addr, port: u16, req: &str) -> HttpResp {
        let mut sock =
            TcpStream::connect_timeout(&SocketAddr::from((ip, port)), Duration::from_secs(2))
                .unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        sock.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).unwrap();
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n");
        let (header_bytes, body) = match split {
            Some(i) => (buf[..i].to_vec(), buf[i + 4..].to_vec()),
            None => (buf.clone(), Vec::new()),
        };
        let headers = String::from_utf8_lossy(&header_bytes).into_owned();
        let status: u16 = headers
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        HttpResp {
            status,
            headers,
            body,
        }
    }

    const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";

    #[derive(Clone, Copy)]
    struct FakeScript {
        launch_error: Option<&'static str>,
        load_failed: bool,
        /// playerState on the LOAD reply.
        load_state: &'static str,
        /// Unsolicited MEDIA_STATUS frames sent after the LOAD reply (requestId 0).
        follow_states: &'static [&'static str],
        /// After the Nth successful LOAD (1-based), send IDLE with this idleReason.
        /// Session 0 means the loaded session (7). A non-matching id is a stale item.
        idle_after: &'static [(u32, &'static str, i32)],
        /// When non-zero, `idle_after` waits this long after the LOAD instead of
        /// being sent inside the LOAD handler. Zero keeps the immediate send.
        idle_delay: Duration,
        /// A still image. LOAD is answered with BUFFERING on the LOAD requestId.
        /// Unsolicited requestId 0 statuses arrive before and after that reply.
        /// PLAY and PAUSE are not answered.
        still: bool,
        /// PLAY is left unanswered. PAUSE still gets a status.
        silence_play: bool,
        /// SEEK is left unanswered.
        silence_seek: bool,
        /// LOAD is answered only with ERROR and no requestId.
        load_bare_error: bool,
        /// A media command is first sent ERROR with no requestId, then its real reply.
        media_bare_error: bool,
        /// PLAY is answered with this type instead of MEDIA_STATUS.
        /// `ERROR` is a raw reply with no detailedErrorCode.
        play_reply: Option<&'static str>,
    }

    impl Default for FakeScript {
        fn default() -> Self {
            Self {
                launch_error: None,
                load_failed: false,
                load_state: "PLAYING",
                follow_states: &[],
                idle_after: &[],
                idle_delay: Duration::ZERO,
                still: false,
                silence_play: false,
                silence_seek: false,
                load_bare_error: false,
                media_bare_error: false,
                play_reply: None,
            }
        }
    }

    struct FakeTv {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        traces: Arc<Mutex<Vec<String>>>,
        /// contentId strings taken from LOAD frames, in order.
        content_ids: Arc<Mutex<Vec<String>>>,
        join: Option<JoinHandle<()>>,
    }

    impl FakeTv {
        fn host_port(&self) -> (Ipv4Addr, u16) {
            match self.addr.ip() {
                IpAddr::V4(ip) => (ip, self.addr.port()),
                _ => (Ipv4Addr::LOCALHOST, self.addr.port()),
            }
        }

        fn trace_text(&self) -> String {
            self.traces
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .join("\n")
        }

        fn content_ids(&self) -> Vec<String> {
            self.content_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    impl Drop for FakeTv {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(300));
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    struct LogDir(PathBuf);

    impl LogDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("slidex-cast-log-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            CAST_LOG_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(dir.clone()));
            Self(dir)
        }

        fn text(&self) -> String {
            std::fs::read_to_string(self.0.join("cast.log")).unwrap_or_default()
        }
    }

    impl Drop for LogDir {
        fn drop(&mut self) {
            CAST_LOG_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn note_trace(traces: &Mutex<Vec<String>>, line: &str) {
        if let Ok(mut guard) = traces.lock() {
            guard.push(line.to_string());
        }
    }

    fn fake_server_config() -> Arc<rustls::ServerConfig> {
        let _ = default_provider().install_default();
        let cert = CertificateDer::from(include_bytes!("cast-fake/cert.der").to_vec());
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                include_bytes!("cast-fake/key.der").to_vec(),
            ));
        Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .expect("fake cast certificate"),
        )
    }

    fn fake_send<S: Read + Write>(
        mm: &MessageManager<S>,
        namespace: &str,
        source: &str,
        payload: &str,
    ) {
        let _ = mm.send(CastMessage {
            namespace: namespace.to_string(),
            source: source.to_string(),
            destination: "sender-0".to_string(),
            payload: CastMessagePayload::String(payload.to_string()),
        });
    }

    fn send_receiver_status<S: Read + Write>(
        mm: &MessageManager<S>,
        request_id: u32,
        traces: &Mutex<Vec<String>>,
    ) {
        // Record before the write so a fast client cannot observe the frame first.
        note_trace(traces, "receiver-status");
        let decoy = serde_json::json!({
            "requestId": 999,
            "type": "RECEIVER_STATUS",
            "status": {
                "applications": [{
                    "appId": "CC1AD845",
                    "sessionId": "session-wrong",
                    "transportId": "wrong-transport",
                    "namespaces": [{"name": NS_MEDIA}],
                    "displayName": "Wrong",
                    "statusText": "ignore"
                }],
                "volume": {"level": 1.0, "muted": false}
            }
        });
        fake_send(mm, NS_RECEIVER, "receiver-0", &decoy.to_string());
        let real = serde_json::json!({
            "requestId": request_id,
            "type": "RECEIVER_STATUS",
            "status": {
                "applications": [{
                    "appId": "CC1AD845",
                    "sessionId": "session-1",
                    "transportId": "transport-1",
                    "namespaces": [{"name": NS_MEDIA}],
                    "displayName": "Default Media Receiver",
                    "statusText": "SlideX"
                }],
                "isActiveInput": true,
                "isStandBy": false,
                "volume": {"level": 1.0, "muted": false}
            }
        });
        fake_send(mm, NS_RECEIVER, "receiver-0", &real.to_string());
    }

    fn send_media_status<S: Read + Write>(
        mm: &MessageManager<S>,
        request_id: u32,
        session: i32,
        player_state: &str,
    ) {
        send_media_status_idle(mm, request_id, session, player_state, None);
    }

    fn send_media_status_idle<S: Read + Write>(
        mm: &MessageManager<S>,
        request_id: u32,
        session: i32,
        player_state: &str,
        idle_reason: Option<&str>,
    ) {
        let mut status = serde_json::json!({
            "mediaSessionId": session,
            "playbackRate": 1.0,
            "playerState": player_state,
            "currentTime": 0.0,
            "supportedMediaCommands": 15
        });
        if let Some(reason) = idle_reason {
            status["idleReason"] = serde_json::Value::String(reason.to_string());
        }
        let body = serde_json::json!({
            "requestId": request_id,
            "type": "MEDIA_STATUS",
            "status": [status]
        });
        fake_send(mm, NS_MEDIA, "transport-1", &body.to_string());
    }

    struct DueIdle {
        at: Instant,
        reason: &'static str,
        session: i32,
    }

    fn flush_due_idles<S: Read + Write>(
        mm: &MessageManager<S>,
        traces: &Mutex<Vec<String>>,
        due: &mut Vec<DueIdle>,
    ) {
        let now = Instant::now();
        let mut i = 0;
        while i < due.len() {
            if due[i].at > now {
                i += 1;
                continue;
            }
            let idle = due.remove(i);
            note_trace(traces, &format!("idle {} session={}", idle.reason, idle.session));
            send_media_status_idle(mm, 0, idle.session, "IDLE", Some(idle.reason));
        }
    }

    fn fake_session<S: Read + Write>(
        mm: &MessageManager<S>,
        script: FakeScript,
        traces: &Mutex<Vec<String>>,
        content_ids: &Mutex<Vec<String>>,
        stop: &AtomicBool,
    ) {
        let mut receiver_up = false;
        let mut transport_up = false;
        let mut awaiting_pong = false;
        let mut launch_id = 0u32;
        let mut loads = 0u32;
        let mut due: Vec<DueIdle> = Vec::new();
        let mut image_session = false;
        while !stop.load(Ordering::SeqCst) {
            flush_due_idles(mm, traces, &mut due);
            let msg = match mm.receive() {
                Ok(msg) => msg,
                Err(e) if cast_err_is_timeout(&e) => continue,
                Err(_) => break,
            };
            let payload = match &msg.payload {
                CastMessagePayload::String(text) => text.clone(),
                _ => continue,
            };
            let value: serde_json::Value =
                serde_json::from_str(&payload).unwrap_or(serde_json::json!({}));
            let typ = value
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if !receiver_up {
                if typ == "CONNECT" && msg.destination == "receiver-0" {
                    receiver_up = true;
                    note_trace(traces, "connect-receiver");
                } else {
                    note_trace(
                        traces,
                        &format!("refused-before-connect {typ} {}", msg.destination),
                    );
                }
                continue;
            }
            if typ == "PING" {
                note_trace(traces, "ping");
                fake_send(mm, NS_HEARTBEAT, "receiver-0", r#"{"type":"PONG"}"#);
                note_trace(traces, "pong-sent");
                continue;
            }
            if typ == "PONG" {
                note_trace(traces, "client-pong");
                if awaiting_pong {
                    awaiting_pong = false;
                    send_receiver_status(mm, launch_id, traces);
                }
                continue;
            }
            if typ == "CONNECT" && msg.destination == "transport-1" {
                transport_up = true;
                note_trace(traces, "connect-transport");
                continue;
            }
            if typ == "LAUNCH" {
                let id = value.get("requestId").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                note_trace(traces, &format!("launch {id}"));
                if let Some(reason) = script.launch_error {
                    let body = serde_json::json!({
                        "requestId": id,
                        "type": "LAUNCH_ERROR",
                        "reason": reason,
                    });
                    note_trace(traces, "launch-error");
                    fake_send(mm, NS_RECEIVER, "receiver-0", &body.to_string());
                    continue;
                }
                launch_id = id;
                awaiting_pong = true;
                fake_send(mm, NS_HEARTBEAT, "receiver-0", r#"{"type":"PING"}"#);
                note_trace(traces, "launch-ping");
                continue;
            }
            if typ == "LOAD" {
                let id = value.get("requestId").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let dest = msg.destination.clone();
                if dest == "receiver-0" || !transport_up || dest != "transport-1" {
                    note_trace(traces, &format!("load-refused {dest}"));
                    continue;
                }
                loads += 1;
                let content_id = value
                    .pointer("/media/contentId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Ok(mut ids) = content_ids.lock() {
                    ids.push(content_id.clone());
                }
                let item = content_id.rsplit('/').next().unwrap_or("");
                let ctype = value
                    .pointer("/media/contentType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if ctype.starts_with("image/") {
                    image_session = true;
                } else if ctype.starts_with("video/") {
                    image_session = false;
                }
                let autoplay = value.get("autoplay").and_then(|v| v.as_bool()).unwrap_or(false);
                note_trace(traces, &format!("load {dest} item={item}"));
                note_trace(
                    traces,
                    if autoplay {
                        "autoplay=true"
                    } else {
                        "autoplay=false"
                    },
                );
                if script.load_bare_error {
                    // detailedErrorCode is what rust_cast needs to parse ERROR.
                    // The parsed error still has no requestId.
                    note_trace(traces, "bare-error");
                    fake_send(
                        mm,
                        NS_MEDIA,
                        "transport-1",
                        r#"{"type":"ERROR","detailedErrorCode":104}"#,
                    );
                    continue;
                }
                if script.load_failed {
                    let body = serde_json::json!({
                        "requestId": id,
                        "type": "LOAD_FAILED",
                    });
                    note_trace(traces, "load-failed");
                    fake_send(mm, NS_MEDIA, "transport-1", &body.to_string());
                    continue;
                }
                if script.still {
                    // requestId 0, and a different mediaSessionId, must not satisfy LOAD.
                    note_trace(traces, "media-status requestId=0");
                    send_media_status(mm, 0, 1, "PAUSED");
                    note_trace(traces, "media-status");
                    send_media_status(mm, id, 7, "BUFFERING");
                    note_trace(traces, "media-status requestId=0");
                    send_media_status(mm, 0, 7, "PAUSED");
                    continue;
                }
                // requestId 0 is unsolicited. It must not satisfy the LOAD wait.
                note_trace(traces, "media-status requestId=0");
                send_media_status(mm, 0, 1, "PAUSED");
                note_trace(traces, "media-status");
                send_media_status(mm, 999, 1, "PLAYING");
                note_trace(traces, &format!("media-status requestId={id}"));
                send_media_status(mm, id, 7, script.load_state);
                for state in script.follow_states {
                    send_media_status(mm, 0, 7, state);
                }
                for &(n, reason, session) in script.idle_after {
                    if n != loads {
                        continue;
                    }
                    let sid = if session == 0 { 7 } else { session };
                    if script.idle_delay.is_zero() {
                        note_trace(traces, &format!("idle {reason} session={sid}"));
                        send_media_status_idle(mm, 0, sid, "IDLE", Some(reason));
                    } else {
                        due.push(DueIdle {
                            at: Instant::now() + script.idle_delay,
                            reason,
                            session: sid,
                        });
                    }
                }
                continue;
            }
            if typ == "CLOSE" {
                note_trace(traces, &format!("CLOSE {}", msg.destination));
                continue;
            }
            if typ == "STOP" && msg.namespace == NS_RECEIVER {
                let sid = value
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let id = value.get("requestId").and_then(|v| v.as_u64()).unwrap_or(0);
                note_trace(
                    traces,
                    &format!("STOP {} session={sid} requestId={id}", msg.destination),
                );
                continue;
            }
            if matches!(typ.as_str(), "PAUSE" | "PLAY" | "STOP" | "SEEK") {
                let id = value.get("requestId").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let sid = value
                    .get("mediaSessionId")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32;
                let dest = msg.destination.clone();
                note_trace(
                    traces,
                    &format!("{typ} {dest} session={sid} requestId={id}"),
                );
                if dest != "transport-1" || !transport_up {
                    note_trace(traces, &format!("media-refused {typ} {dest}"));
                    continue;
                }
                if script.still && matches!(typ.as_str(), "PLAY" | "PAUSE") {
                    note_trace(traces, &format!("{typ} {dest} session={sid} requestId={id} unanswered"));
                    continue;
                }
                // A photo LOAD is answered with MEDIA_STATUS. PLAY for that image is not.
                if image_session && typ == "PLAY" {
                    note_trace(traces, &format!("{typ} {dest} session={sid} requestId={id} unanswered"));
                    continue;
                }
                if script.silence_play && typ == "PLAY" {
                    note_trace(traces, &format!("{typ} {dest} session={sid} requestId={id} unanswered"));
                    continue;
                }
                if script.silence_seek && typ == "SEEK" {
                    note_trace(traces, &format!("{typ} {dest} session={sid} requestId={id} unanswered"));
                    continue;
                }
                if script.media_bare_error {
                    note_trace(traces, "bare-error");
                    fake_send(
                        mm,
                        NS_MEDIA,
                        "transport-1",
                        r#"{"type":"ERROR","detailedErrorCode":104}"#,
                    );
                }
                if typ == "PLAY" {
                    if let Some(kind) = script.play_reply {
                        let body = match kind {
                            "ERROR" => serde_json::json!({
                                "requestId": id,
                                "type": "ERROR",
                                "reason": "INVALID_MEDIA",
                            }),
                            "INVALID_REQUEST" => serde_json::json!({
                                "requestId": id,
                                "type": "INVALID_REQUEST",
                                "reason": "INVALID_MEDIA",
                            }),
                            _ => serde_json::json!({
                                "requestId": id,
                                "type": kind,
                            }),
                        };
                        note_trace(traces, &format!("play-reply {kind}"));
                        fake_send(mm, NS_MEDIA, "transport-1", &body.to_string());
                        continue;
                    }
                }
                let state = match typ.as_str() {
                    "PAUSE" => "PAUSED",
                    "STOP" => "IDLE",
                    _ => "PLAYING",
                };
                if typ == "PLAY" {
                    // A video resume. requestId 0 must not satisfy this PLAY.
                    note_trace(traces, "media-status requestId=0");
                    send_media_status(mm, 0, 1, "PAUSED");
                }
                note_trace(traces, &format!("media-status requestId={id}"));
                send_media_status(mm, id, sid, state);
            }
        }
    }

    fn spawn_fake(script: FakeScript) -> FakeTv {
        let _ = default_provider().install_default();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let traces = Arc::new(Mutex::new(Vec::new()));
        let content_ids = Arc::new(Mutex::new(Vec::new()));
        let stop_t = Arc::clone(&stop);
        let traces_t = Arc::clone(&traces);
        let content_ids_t = Arc::clone(&content_ids);
        let config = fake_server_config();
        let join = thread::spawn(move || {
            while !stop_t.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((tcp, _)) => {
                        tcp.set_nodelay(true).ok();
                        tcp.set_read_timeout(None).ok();
                        tcp.set_write_timeout(None).ok();
                        tcp.set_nonblocking(true).ok();
                        let Ok(conn) = rustls::ServerConnection::new(Arc::clone(&config)) else {
                            note_trace(&traces_t, "tls-config");
                            continue;
                        };
                        let pace = Arc::new(Mutex::new(READ_SLICE));
                        let plain =
                            CastPlain::new(StreamOwned::new(conn, IdleSocket { inner: tcp }), pace);
                        let mm = MessageManager::new(plain);
                        fake_session(&mm, script, traces_t.as_ref(), content_ids_t.as_ref(), &stop_t);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(15));
                    }
                    Err(_) => break,
                }
            }
        });
        FakeTv {
            addr,
            stop,
            traces,
            content_ids,
            join: Some(join),
        }
    }

    #[test]
    fn heartbeat_deadlines_and_launch_timeout_sentence() {
        assert_eq!(PING_EVERY, Duration::from_secs(5));
        assert_eq!(LINK_DEAD_AFTER, Duration::from_secs(15));
        assert_eq!(LAUNCH_DEADLINE, Duration::from_secs(15));
        assert_eq!(LOAD_DEADLINE, Duration::from_secs(15));
        assert!(SESSION_READY_WAIT > LAUNCH_DEADLINE);
        assert_eq!(
            map_launch_failure(
                "A connection attempt failed because the connected party did not properly respond after a period of time, or established connection failed because connected host has failed to respond. (os error 10060)"
            ),
            LAUNCH_TIMEOUT_MESSAGE
        );
        let refused = "The TV refused to open the player (NOT_FOUND).";
        assert_eq!(map_launch_failure(refused), refused);
        let timed_out = std::io::Error::new(ErrorKind::TimedOut, "os error 10060");
        assert!(io_is_idle(&timed_out));
        assert!(io_is_idle(&std::io::Error::new(
            ErrorKind::WouldBlock,
            "would block"
        )));
        assert!(io_is_idle(&std::io::Error::from_raw_os_error(10060)));
        assert!(!io_is_idle(&std::io::Error::new(
            ErrorKind::Other,
            "timeout"
        )));
        assert!(!io_is_idle(&std::io::Error::new(
            ErrorKind::Other,
            "os error 110"
        )));
        assert_eq!(LAN_CATEGORY_WAIT, Duration::from_millis(2500));
        assert_eq!(unix_to_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(
            media_url_host("http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/1")
                .as_deref(),
            Some("192.168.1.52:47200")
        );
    }

    #[test]
    fn hung_profile_query_returns_unknown_within_the_deadline() {
        let limit = Duration::from_millis(800);
        let started = Instant::now();
        let category =
            net_category_from_profile_command(Some("Wi-Fi"), hanging_process_command(), limit);
        let elapsed = started.elapsed();
        assert_eq!(category, NetCategory::Unknown);
        assert!(
            elapsed < limit + Duration::from_secs(4),
            "hung profile query took {elapsed:?}"
        );
        assert!(
            elapsed + Duration::from_millis(250) >= limit,
            "returned in {elapsed:?} without waiting out the deadline"
        );
    }

    #[test]
    fn write_stall_mid_frame_keeps_the_bytes_and_finishes() {
        struct Stall {
            buf: Vec<u8>,
            stall_after: usize,
            blocks_left: usize,
        }
        impl Write for Stall {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                if self.buf.len() >= self.stall_after && self.blocks_left > 0 {
                    self.blocks_left -= 1;
                    return Err(ErrorKind::WouldBlock.into());
                }
                let n = data.len().min(5);
                self.buf.extend_from_slice(&data[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let body = b"cast-frame-body";
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(body);
        let mut sink = Stall {
            buf: Vec::new(),
            stall_after: 4,
            blocks_left: 3,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        let started = Instant::now();
        let mut rest = frame.as_slice();
        while !rest.is_empty() {
            let n = write_some_retry_idle(&mut sink, rest, deadline).expect("mid-frame stall");
            rest = &rest[n..];
        }
        assert_eq!(sink.buf, frame, "bytes accepted before the stall must stay");
        assert_eq!(
            sink.blocks_left, 0,
            "the write must stall after the length prefix"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cast_log_is_under_localappdata_names_the_lan_host_and_stays_under_one_megabyte() {
        let base = Path::new(r"C:\Users\qa\AppData\Local");
        let path = cast_log_path_in(base);
        assert_eq!(
            path.strip_prefix(base).unwrap(),
            Path::new("com.johnalindogan.slideshowpro")
                .join("logs")
                .join("cast.log")
        );
        assert!(path.ends_with("cast.log"));
        let dir = LogDir::new();
        let token = "0123456789abcdef0123456789abcdef";
        let host = media_url_host(&format!("http://192.168.1.52:47200/m/{token}/1")).unwrap();
        cast_log("load", &host);
        cast_log("load", &format!("http://192.168.1.52:47200/m/{token}/1"));
        cast_log("load", r"D:\Photos\vacation.jpg");
        let text = dir.text();
        assert!(text.contains("load 192.168.1.52:47200"), "{text}");
        assert!(!text.contains(token), "{text}");
        assert!(!text.contains("vacation"), "{text}");
        assert!(text.contains("redacted"), "{text}");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(dir.0.join("cast.log"))
                .unwrap();
            file.write_all(&vec![b'x'; CAST_LOG_MAX as usize]).unwrap();
        }
        cast_log("connect", "127.0.0.1:8009");
        let current = std::fs::metadata(dir.0.join("cast.log")).unwrap().len();
        let rotated = std::fs::metadata(dir.0.join("cast.log.1")).unwrap().len();
        assert!(current < CAST_LOG_MAX, "cast.log is {current} bytes");
        assert_eq!(rotated, CAST_LOG_MAX);
        let fresh = dir.text();
        assert!(fresh.contains("127.0.0.1:8009"), "{fresh}");
        assert!(!fresh.contains(&"x".repeat(80)), "{fresh}");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(dir.0.join("cast.log"))
                .unwrap();
            file.write_all(&vec![b'y'; CAST_LOG_MAX as usize + 64])
                .unwrap();
        }
        cast_log("timeout", "past-one-megabyte");
        let past = std::fs::metadata(dir.0.join("cast.log")).unwrap().len();
        let past_rotated = std::fs::metadata(dir.0.join("cast.log.1")).unwrap().len();
        assert!(past < CAST_LOG_MAX, "cast.log is {past} bytes");
        assert_eq!(past_rotated, CAST_LOG_MAX + 64);
        let after = dir.text();
        assert!(after.contains("past-one-megabyte"), "{after}");
        assert!(!after.contains(&"y".repeat(80)), "{after}");
    }

    #[test]
    fn fake_tv_refuses_anything_sent_before_connect() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::open_tls(ip, port).unwrap();
        let id = link.mm.generate_request_id().get();
        link.send_json(NS_RECEIVER, "receiver-0", &launch_payload(id))
            .unwrap();
        let err = link
            .wait_matching(
                Duration::from_millis(700),
                WaitMsg::Text("no-reply-before-connect"),
                WaitExpect::Launch(id),
            )
            .unwrap_err();
        assert_eq!(err, "no-reply-before-connect");
        link.ping().unwrap();
        assert!(
            link.pump().unwrap().is_none(),
            "PING before CONNECT must get no PONG"
        );
        link.connection.connect("receiver-0").unwrap();
        link.launch_receiver().unwrap();
        assert_eq!(link.transport_id, "transport-1");
        let text = fake.trace_text();
        let refused_launch = text.find("refused-before-connect LAUNCH").expect(&text);
        let refused_ping = text.find("refused-before-connect PING").expect(&text);
        let connected = text.find("connect-receiver").expect(&text);
        assert!(refused_launch < connected, "{text}");
        assert!(refused_ping < connected, "{text}");
        let pong = text.find("client-pong").expect(&text);
        let status = text.find("receiver-status").expect(&text);
        assert!(pong < status, "{text}");
        let written = log.text();
        assert!(
            written.contains("timeout") && written.contains("no-reply-before-connect"),
            "{written}"
        );
        assert_eq!(
            written.lines().filter(|l| l.contains(" ping ")).count(),
            1,
            "{written}"
        );
        assert_eq!(
            written.lines().filter(|l| l.contains(" pong ")).count(),
            1,
            "{written}"
        );
    }

    #[test]
    fn cast_link_connects_before_launch_and_loads_the_transport() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        assert_eq!(link.transport_id, "transport-1");
        assert_eq!(link.session_id, "session-1");
        let token = "0123456789abcdef0123456789abcdef";
        let url = format!("http://192.168.1.52:47200/m/{token}/1");
        let sid = link
            .load_url(&url, "image/jpeg", true, CastOrigin::User, 1, "photo")
            .unwrap();
        assert_eq!(sid, 7);
        let text = fake.trace_text();
        assert!(!text.contains("refused-before-connect"), "{text}");
        let connect_at = text.find("connect-receiver").expect(&text);
        let launch_at = text.find("launch ").expect(&text);
        assert!(connect_at < launch_at, "{text}");
        let pong = text.find("client-pong").expect(&text);
        let status = text.find("receiver-status").expect(&text);
        assert!(pong < status, "{text}");
        assert!(text.contains("connect-transport"), "{text}");
        assert!(text.contains("load transport-1"), "{text}");
        assert!(!text.contains("load-refused"), "{text}");
        let written = log.text();
        assert!(written.contains("receiver-connect receiver-0"), "{written}");
        assert!(written.contains("load 192.168.1.52:47200"), "{written}");
        assert!(written.contains("item=1 type=photo"), "{written}");
        assert!(!written.contains(token), "{written}");
        assert!(!written.contains("vacation"), "{written}");
        link.ping().unwrap();
        let after_ping = log.text();
        assert_eq!(
            after_ping.lines().filter(|l| l.contains(" ping ")).count(),
            1,
            "{after_ping}"
        );
        assert!(after_ping.contains(" pong "), "{after_ping}");
        assert!(after_ping.contains("requestId="), "{after_ping}");
        assert!(after_ping.contains("reply=MEDIA_STATUS"), "{after_ping}");
        assert!(after_ping.contains("playerState=PLAYING"), "{after_ping}");
        assert!(after_ping.contains("idleReason=none"), "{after_ping}");
        assert!(after_ping.contains("mediaSessionId=7"), "{after_ping}");
        match link.pump().unwrap() {
            Some(update) => assert!(update.player.is_none()),
            None => panic!("a PONG from the TV must count as the TV speaking"),
        }
    }

    #[test]
    fn fake_tv_refuses_load_addressed_to_receiver_0() {
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let id = link.mm.generate_request_id().get();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/2";
        link.send_json(
            NS_MEDIA,
            "receiver-0",
            &load_payload(id, &link.session_id, url, "image/jpeg", true),
        )
        .unwrap();
        let err = link
            .wait_matching(
                Duration::from_millis(800),
                WaitMsg::Text("load-to-receiver-0-unanswered"),
                WaitExpect::Load(id),
            )
            .unwrap_err();
        assert_eq!(err, "load-to-receiver-0-unanswered");
        assert!(
            fake.trace_text().contains("load-refused receiver-0"),
            "{}",
            fake.trace_text()
        );
        let sid = link
            .load_url(url, "image/jpeg", true, CastOrigin::User, 1, "photo")
            .unwrap();
        assert_eq!(sid, 7);
        assert!(
            fake.trace_text().contains("load transport-1"),
            "{}",
            fake.trace_text()
        );
    }

    #[test]
    fn launch_error_is_not_reported_as_a_timeout() {
        let fake = spawn_fake(FakeScript {
            launch_error: Some("NOT_FOUND"),
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        let started = Instant::now();
        let err = link.launch_receiver().unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "LAUNCH_ERROR took {:?}",
            started.elapsed()
        );
        assert_eq!(err, "The TV refused to open the player (NOT_FOUND).");
        assert_ne!(err, LAUNCH_TIMEOUT_MESSAGE);
        assert!(!err.to_ascii_lowercase().contains("timed out"));
        assert!(!err.contains("didn't answer"));
        assert!(
            fake.trace_text().contains("launch-error"),
            "{}",
            fake.trace_text()
        );
    }

    #[test]
    fn load_failed_is_not_reported_as_a_timeout() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            load_failed: true,
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let started = Instant::now();
        let err = link
            .load_url(
                "http://192.168.1.52:47201/m/tokentokentokentokentokentoken/3",
                "image/jpeg",
                true,
                CastOrigin::User,
                1,
                "photo",
            )
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "LOAD_FAILED took {:?}",
            started.elapsed()
        );
        assert_eq!(err, MEDIA_FETCH_MESSAGE);
        assert_ne!(err, UNSUPPORTED_MEDIA);
        assert!(!err.contains("15 seconds"));
        assert!(!err.to_ascii_lowercase().contains("timed out"));
        assert!(!err.to_ascii_lowercase().contains("unreachable"));
        assert!(!err.contains("didn't answer"));
        assert!(
            fake.trace_text().contains("load-failed"),
            "{}",
            fake.trace_text()
        );
        let written = log.text();
        assert!(written.contains("reply=LOAD_FAILED"), "{written}");
        assert!(written.contains("playerState=none"), "{written}");
        assert!(written.contains("idleReason=none"), "{written}");
        assert!(
            written.contains("url=http://192.168.1.52:47201/m/3?t=***"),
            "{written}"
        );
        assert!(written.contains("contentType=image/jpeg"), "{written}");
        assert!(!written.contains("tokentokentokentokentokentoken"), "{written}");
        assert!(!written.contains('\\'), "{written}");
    }

    fn session_after_load(kind: &str) -> (Arc<Mutex<Option<String>>>, SessionFlags) {
        let remote = Arc::new(Mutex::new(None));
        let flags = SessionFlags {
            connected: true,
            player_state: "playing".into(),
            remote_event: Arc::clone(&remote),
            error: None,
            media_kind: kind.into(),
            local_cmd: Some("play"),
            local_cmd_at: Instant::now(),
            last_rx: Instant::now(),
            media_session: Some(7),
            disconnect_origin: None,
            advanced_session: None,
            item_index: 1,
            failed_session: None,
            slideshow_playing: true,
            ignored_photo: None,
        };
        (remote, flags)
    }

    fn pump_player_states(link: &CastLink, n: usize) -> Vec<PlayerState> {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while out.len() < n && Instant::now() < deadline {
            match link.pump() {
                Ok(Some(update)) => {
                    if let Some(state) = update.player {
                        out.push(state);
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        out
    }

    struct Driven {
        log: LogDir,
        fake: FakeTv,
        http: MediaHttp,
        link: CastLink,
        status: Mutex<CastLiveStatus>,
        flags: SessionFlags,
        running: bool,
    }

    impl Driven {
        fn load(script: FakeScript, name: &str, bytes: &[u8], autoplay: bool) -> Self {
            let log = LogDir::new();
            let path = scratch(name, bytes);
            let mut http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
            http.set_playlist(std::slice::from_ref(&path)).unwrap();
            let fake = spawn_fake(script);
            let (ip, port) = fake.host_port();
            let mut link = CastLink::connect(ip, port).unwrap();
            link.launch_receiver().unwrap();
            let status = Mutex::new(CastLiveStatus {
                connected: true,
                player_state: "idle".into(),
                remote_event: None,
                error: None,
                media_kind: String::new(),
                http_port: http.port,
                bind_ip: http.bind_ip.to_string(),
                item_index: 0,
            });
            let (_remote, mut flags) = session_after_load("still");
            flags.media_kind.clear();
            flags.media_session = None;
            flags.player_state = "idle".into();
            flags.item_index = 0;
            let mut running = true;
            let (tx, rx) = mpsc::channel();
            let kept = handle_cmd(
                CastCmd::Load {
                    path,
                    autoplay,
                    origin: CastOrigin::User,
                    duration_ms: None,
                    reply: tx,
                },
                &mut http,
                &link,
                &mut flags,
                &status,
                &mut running,
            );
            assert!(kept);
            assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().is_ok());
            Self {
                log,
                fake,
                http,
                link,
                status,
                flags,
                running,
            }
        }

        fn command(&mut self, play: bool) -> (Duration, Result<(), String>) {
            let started = Instant::now();
            let (tx, rx) = mpsc::channel();
            let cmd = if play {
                CastCmd::Play {
                    origin: CastOrigin::User,
                    reply: tx,
                }
            } else {
                CastCmd::Pause {
                    origin: CastOrigin::User,
                    reply: tx,
                }
            };
            let kept = handle_cmd(
                cmd,
                &mut self.http,
                &self.link,
                &mut self.flags,
                &self.status,
                &mut self.running,
            );
            assert!(kept);
            let result = rx.recv_timeout(Duration::from_secs(6)).unwrap();
            (started.elapsed(), result)
        }
    }

    #[test]
    fn photo_play_does_not_wait_when_the_receiver_leaves_play_unanswered() {
        // c5542f0 loaded the photo, then sent PLAY and timed out at 4s because
        // the TV never answers PLAY for an image. LOAD with autoplay is enough.
        let mut cast = Driven::load(
            FakeScript::default(),
            "vacation-photo.jpg",
            &jpeg_bytes(),
            true,
        );
        assert_eq!(cast.flags.media_session, Some(7));
        assert_eq!(cast.flags.media_kind, "still");
        assert_eq!(cast.flags.player_state, "playing");
        let (elapsed, played) = cast.command(true);
        assert!(
            elapsed < Duration::from_millis(1500),
            "photo PLAY waited {elapsed:?}; LOAD then PLAY on a photo must not time out"
        );
        assert!(played.is_ok(), "{played:?}");
        let live = cast.status.lock().unwrap().clone();
        assert_eq!(live.player_state, "playing");
        assert!(live.error.is_none(), "{:?}", live.error);
        assert!(live.connected);
        assert!(cast.flags.slideshow_playing);
        let text = cast.fake.trace_text();
        assert!(text.contains("load transport-1"), "{text}");
        assert!(text.contains("autoplay=true"), "{text}");
        assert!(text.contains("media-status"), "{text}");
        assert!(
            !text.lines().any(|line| line.starts_with("PLAY ")),
            "a still must not be sent PLAY: {text}"
        );
        let written = cast.log.text();
        assert!(written.contains("reply=MEDIA_STATUS"), "{written}");
        assert!(written.contains("playerState=PLAYING"), "{written}");
        assert!(written.contains("mediaSessionId=7"), "{written}");
        assert!(written.contains("play_skipped reason=photo"), "{written}");
        assert!(
            !written.contains("timeout")
                && !written.contains("didn't answer")
                && !written.contains("TV unreachable"),
            "{written}"
        );
    }

    #[test]
    fn video_play_is_sent_only_to_resume_from_pause() {
        let mut cast = Driven::load(
            FakeScript::default(),
            "clip.mp4",
            &mp4_with(&[b"avc1", b"mp4a"]),
            true,
        );
        assert_eq!(cast.flags.media_kind, "video");
        assert_eq!(cast.flags.player_state, "playing");
        let (elapsed, played) = cast.command(true);
        assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
        assert!(played.is_ok(), "{played:?}");
        assert!(
            !cast.fake.trace_text().lines().any(|line| line.starts_with("PLAY ")),
            "{}",
            cast.fake.trace_text()
        );
        assert!(
            cast.log.text().contains("play_skipped reason=not_paused"),
            "{}",
            cast.log.text()
        );
        let (elapsed, paused) = cast.command(false);
        assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
        assert!(paused.is_ok(), "{paused:?}");
        assert_eq!(cast.flags.player_state, "paused");
        assert!(
            cast.fake.trace_text().contains("PAUSE transport-1"),
            "{}",
            cast.fake.trace_text()
        );
        let (elapsed, played) = cast.command(true);
        assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
        assert!(played.is_ok(), "{played:?}");
        assert!(
            cast.fake.trace_text().lines().any(|line| line.starts_with("PLAY ")),
            "{}",
            cast.fake.trace_text()
        );
        assert_eq!(cast.flags.player_state, "playing");
    }

    #[test]
    fn request_id_zero_does_not_satisfy_load_or_video_play() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/clip";
        let sid = link
            .load_url(url, "video/mp4", true, CastOrigin::User, 1, "video")
            .unwrap();
        assert_eq!(sid, 7, "requestId 0 must not win the LOAD");
        let written = log.text();
        let load_line = written
            .lines()
            .find(|line| line.contains(" load "))
            .unwrap_or_else(|| panic!("{written}"));
        assert!(load_line.contains("reply=MEDIA_STATUS"), "{load_line}");
        assert!(load_line.contains("playerState=PLAYING"), "{load_line}");
        assert!(load_line.contains("mediaSessionId=7"), "{load_line}");
        assert!(!load_line.contains("playerState=PAUSED"), "{load_line}");
        assert!(!load_line.contains("requestId=0"), "{load_line}");
        let text = fake.trace_text();
        assert_status_after_zero(&text, "load ", "LOAD");

        assert_eq!(
            link.pause(7, CastOrigin::User).unwrap(),
            PlayerState::Paused
        );
        let started = Instant::now();
        assert_eq!(
            link.play(7, CastOrigin::User).unwrap(),
            PlayerState::Playing,
            "requestId 0 PAUSED must not satisfy PLAY"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "PLAY waited {:?}",
            started.elapsed()
        );
        let text = fake.trace_text();
        assert_status_after_zero(&text, "PLAY ", "PLAY");
        let written = log.text();
        let play_line = written
            .lines()
            .find(|line| line.contains(" play "))
            .unwrap_or_else(|| panic!("{written}"));
        assert!(play_line.contains("reply=PLAYING"), "{play_line}");
        assert!(!play_line.contains("requestId=0"), "{play_line}");
        assert!(!play_line.contains("PAUSED"), "{play_line}");
    }

    /// The unsolicited `requestId=0` status is traced before the matching reply.
    fn assert_status_after_zero(text: &str, command: &str, label: &str) {
        let lines: Vec<&str> = text.lines().collect();
        let cmd_at = lines
            .iter()
            .rposition(|line| line.starts_with(command))
            .unwrap_or_else(|| panic!("{label} missing in {text}"));
        let zero_at = lines
            .iter()
            .enumerate()
            .position(|(i, line)| i > cmd_at && *line == "media-status requestId=0")
            .unwrap_or_else(|| panic!("{label} sent no requestId 0: {text}"));
        let real_at = lines
            .iter()
            .enumerate()
            .position(|(i, line)| {
                i > zero_at
                    && line.starts_with("media-status requestId=")
                    && *line != "media-status requestId=0"
            })
            .unwrap_or_else(|| panic!("{label} sent no matching status: {text}"));
        assert!(cmd_at < zero_at && zero_at < real_at, "{label}: {text}");
    }

    #[test]
    fn unanswered_video_play_warns_without_marking_the_tv_unreachable() {
        let mut cast = Driven::load(
            FakeScript {
                silence_play: true,
                ..FakeScript::default()
            },
            "clip.mp4",
            &mp4_with(&[b"avc1", b"mp4a"]),
            true,
        );
        cast.flags.player_state = "paused".into();
        let (elapsed, played) = cast.command(true);
        assert!(
            elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(6),
            "unanswered PLAY should wait out the media deadline, not the link, got {elapsed:?}"
        );
        assert!(played.is_ok(), "{played:?}");
        let live = cast.status.lock().unwrap().clone();
        assert!(live.connected);
        assert!(live.error.is_none(), "{:?}", live.error);
        assert_eq!(live.player_state, "playing");
        let written = cast.log.text();
        assert!(written.contains("warning=unanswered"), "{written}");
        assert!(!written.contains("TV unreachable"), "{written}");
    }

    #[test]
    fn error_without_request_id_satisfies_only_the_load_wait() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            load_bare_error: true,
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/clip";
        let started = Instant::now();
        let err = link
            .load_url(url, "video/mp4", true, CastOrigin::User, 1, "video")
            .expect_err("bare ERROR ends the LOAD wait");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "LOAD treated a requestId-less ERROR as silence: {:?}",
            started.elapsed()
        );
        assert_eq!(err, MEDIA_FETCH_MESSAGE);
        let written = log.text();
        assert!(written.contains("reply=ERROR"), "{written}");
        assert!(fake.trace_text().contains("bare-error"), "{}", fake.trace_text());

        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            media_bare_error: true,
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        link.load_url(url, "video/mp4", true, CastOrigin::User, 1, "video")
            .unwrap();
        let started = Instant::now();
        let state = link.pause(7, CastOrigin::User).unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "PAUSE must ignore ERROR with no requestId, waited {:?}",
            started.elapsed()
        );
        assert_eq!(state, PlayerState::Paused);
        let written = log.text();
        let pause = written
            .lines()
            .find(|l| l.contains(" pause "))
            .unwrap_or_else(|| panic!("{written}"));
        assert!(pause.contains("reply=PAUSED"), "{pause}");
        assert!(!pause.contains("error="), "{pause}");
        assert!(fake.trace_text().contains("bare-error"), "{}", fake.trace_text());
    }

    #[test]
    fn unanswered_seek_warns_instead_of_erroring() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            silence_seek: true,
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/clip";
        link.load_url(url, "video/mp4", true, CastOrigin::User, 1, "video")
            .unwrap();
        let started = Instant::now();
        let state = link.seek(7, 1.5, CastOrigin::User).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(6),
            "unanswered SEEK should wait out the media deadline, got {elapsed:?}"
        );
        assert_eq!(state, PlayerState::Playing);
        let written = log.text();
        let seek = written
            .lines()
            .find(|l| l.contains(" seek "))
            .unwrap_or_else(|| panic!("{written}"));
        assert!(seek.contains("warning=unanswered"), "{seek}");
        assert!(!seek.contains("error="), "{seek}");
        assert!(!written.contains("TV unreachable"), "{written}");
        assert!(
            fake.trace_text().contains("SEEK transport-1"),
            "{}",
            fake.trace_text()
        );
    }

    #[test]
    fn play_error_replies_match_the_request_and_stay_connected() {
        for kind in ["ERROR", "LOAD_FAILED", "INVALID_REQUEST"] {
            let mut cast = Driven::load(
                FakeScript {
                    play_reply: Some(kind),
                    ..FakeScript::default()
                },
                "clip.mp4",
                &mp4_with(&[b"avc1", b"mp4a"]),
                true,
            );
            cast.flags.player_state = "paused".into();
            let (elapsed, played) = cast.command(true);
            assert!(
                elapsed < Duration::from_millis(1500),
                "{kind} waited {elapsed:?}"
            );
            let err = played.expect_err(kind);
            assert!(err.contains("refused"), "{kind} {err}");
            assert!(!err.contains("unreachable"), "{kind} {err}");
            assert!(!err.to_ascii_lowercase().contains("timed out"), "{kind} {err}");
            match kind {
                "ERROR" => assert!(err.contains("ERROR"), "{err}"),
                "LOAD_FAILED" => assert!(err.contains("LOAD_FAILED"), "{err}"),
                _ => assert!(
                    err.contains("INVALID_REQUEST") || err.contains("INVALID_MEDIA"),
                    "{err}"
                ),
            }
            let live = cast.status.lock().unwrap().clone();
            assert!(live.connected, "{kind}");
            assert_ne!(live.player_state, "playing", "{kind} {}", live.player_state);
            let shown = live.error.expect(kind);
            assert_eq!(shown, err);
            assert!(!shown.contains("unreachable"), "{shown}");
            assert!(!shown.contains("TV unreachable"), "{shown}");
        }
    }

    #[test]
    fn photo_paused_status_does_not_show_a_remote_pause() {
        let fake = spawn_fake(FakeScript {
            follow_states: &["PAUSED"],
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/9";
        assert_eq!(
            link.load_url(url, "image/jpeg", true, CastOrigin::User, 1, "photo")
                .unwrap(),
            7
        );
        let (remote, mut flags) = session_after_load("still");
        let states = pump_player_states(&link, 1);
        assert_eq!(
            states,
            vec![PlayerState::Paused],
            "the photo must report PAUSED"
        );
        for state in states {
            flags.note_state(state, false);
        }
        assert!(
            remote.lock().unwrap().is_none(),
            "a still image must not raise Paused from the TV remote"
        );
        assert_eq!(flags.player_state, "playing");
    }

    #[test]
    fn video_playing_to_paused_shows_a_remote_pause() {
        let fake = spawn_fake(FakeScript {
            follow_states: &["PLAYING", "PAUSED"],
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/8";
        assert_eq!(
            link.load_url(url, "video/mp4", true, CastOrigin::User, 1, "video")
                .unwrap(),
            7
        );
        let (remote, mut flags) = session_after_load("video");
        let states = pump_player_states(&link, 2);
        assert_eq!(
            states,
            vec![PlayerState::Playing, PlayerState::Paused],
            "the video must go PLAYING to PAUSED"
        );
        for state in states {
            flags.note_state(state, false);
        }
        assert_eq!(remote.lock().unwrap().as_deref(), Some("paused"));
        assert_eq!(flags.player_state, "paused");
    }

    #[test]
    fn pause_play_stop_and_seek_log_the_request_and_the_tv_reply() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/4";
        assert_eq!(
            link.load_url(url, "video/mp4", true, CastOrigin::User, 1, "video")
                .unwrap(),
            7
        );
        assert_eq!(
            link.pause(7, CastOrigin::User).unwrap(),
            PlayerState::Paused
        );
        assert_eq!(
            link.play(7, CastOrigin::User).unwrap(),
            PlayerState::Playing
        );
        assert_eq!(
            link.seek(7, 3.5, CastOrigin::User).unwrap(),
            PlayerState::Playing
        );
        assert_eq!(link.stop(7, CastOrigin::User).unwrap(), PlayerState::Idle);
        link.ping().unwrap();
        let text = fake.trace_text();
        assert!(
            text.contains("PAUSE transport-1 session=7 requestId="),
            "{text}"
        );
        assert!(
            text.contains("PLAY transport-1 session=7 requestId="),
            "{text}"
        );
        assert!(
            text.contains("SEEK transport-1 session=7 requestId="),
            "{text}"
        );
        assert!(
            text.contains("STOP transport-1 session=7 requestId="),
            "{text}"
        );
        let written = log.text();
        for step in ["load ", "pause ", "play ", "seek ", "stop "] {
            let line = written
                .lines()
                .find(|l| l.contains(step))
                .unwrap_or_else(|| panic!("missing {step} in {written}"));
            assert!(line.contains("requestId="), "{line}");
            assert!(line.contains("reply="), "{line}");
            assert!(line.contains("source=user"), "{line}");
        }
        assert!(written.contains("load 192.168.1.52:47200"), "{written}");
        assert!(written.contains("item=1 type=video"), "{written}");
        assert!(written.contains("reply=PAUSED"), "{written}");
        assert!(written.contains("mediaSessionId=7"), "{written}");
        assert!(
            !written.contains("0123456789abcdef0123456789abcdef"),
            "{written}"
        );
        assert_eq!(
            written.lines().filter(|l| l.contains(" ping ")).count(),
            1,
            "{written}"
        );
        assert_eq!(
            written.lines().filter(|l| l.contains(" pong ")).count(),
            1,
            "{written}"
        );
    }

    #[test]
    fn video_cast_with_no_user_input_sends_no_pause_or_stop() {
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let url = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/11";
        assert_eq!(
            link.load_url(url, "video/mp4", true, CastOrigin::User, 1, "video")
                .unwrap(),
            7
        );
        let end = Instant::now() + Duration::from_secs(30);
        let mut next_ping = Instant::now();
        while Instant::now() < end {
            if Instant::now() >= next_ping {
                link.ping().expect("ping");
                next_ping = Instant::now() + PING_EVERY;
            }
            match link.pump() {
                Ok(_) => {}
                Err(e) => panic!("pump during idle video cast: {e}"),
            }
        }
        let text = fake.trace_text();
        assert!(text.contains("load transport-1"), "{text}");
        assert!(!text.contains("PAUSE"), "{text}");
        assert!(!text.contains("STOP"), "{text}");
    }

    #[test]
    fn disconnect_sends_receiver_stop_with_the_session_and_no_load() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        link.cancel.store(true, Ordering::SeqCst);
        let err = link
            .load_url(
                "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/12",
                "video/mp4",
                true,
                CastOrigin::Timer,
                1,
                "video",
            )
            .unwrap_err();
        assert_eq!(err, CAST_CANCELLED);
        link.finish_disconnect(Some(7), CastOrigin::User);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = fake.trace_text();
            if text.contains("CLOSE transport-1") {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!text.contains("load "), "{text}");
        assert!(!text.contains("LOAD"), "{text}");
        let media = text.find("STOP transport-1 session=7").expect(&text);
        let recv = text.find("STOP receiver-0 session=session-1").expect(&text);
        let close = text.find("CLOSE transport-1").expect(&text);
        assert!(media < recv && recv < close, "{text}");
        let lines: Vec<_> = log
            .text()
            .lines()
            .filter(|l| l.contains(" disconnect "))
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines.len(), 1, "{}", log.text());
        assert!(lines[0].contains("source=user"), "{}", lines[0]);
        assert!(lines[0].contains("receiverStop=session-1"), "{}", lines[0]);
        assert!(
            !lines[0].contains('\\') && !lines[0].contains('/'),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn disconnect_logs_one_line_when_the_link_is_already_dead() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript::default());
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        drop(fake);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match link.pump() {
                Ok(_) => {}
                Err(_) => break,
            }
        }
        link.finish_disconnect(Some(7), CastOrigin::User);
        let lines: Vec<_> = log
            .text()
            .lines()
            .filter(|l| l.contains(" disconnect "))
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines.len(), 1, "{}", log.text());
        assert!(lines[0].contains("source=user"), "{}", lines[0]);
        assert!(!lines[0].contains('\\'), "{}", lines[0]);
        assert!(!lines[0].contains('/'), "{}", lines[0]);
        assert!(!lines[0].contains("redacted"), "{}", lines[0]);
    }

    #[test]
    fn failed_connect_logs_the_reason_without_a_path() {
        let log = LogDir::new();
        let err = match CastLink::connect(Ipv4Addr::LOCALHOST, 1) {
            Ok(_) => panic!("port 1 must not accept a cast connection"),
            Err(err) => err,
        };
        assert!(err.contains("TV unreachable"), "{err}");
        assert!(!err.contains('\\') && !err.contains('/'), "{err}");
        let written = log.text();
        let lines: Vec<_> = written.lines().filter(|l| l.contains(" error ")).collect();
        assert_eq!(lines.len(), 1, "{written}");
        assert!(lines[0].contains("TV unreachable at 127.0.0.1"), "{written}");
        assert!(lines[0].contains("reason=\"TV unreachable"), "{written}");
        assert!(lines[0].contains("host=127.0.0.1"), "{written}");
        assert!(!written.contains('\\'), "{written}");
        assert!(!written.contains('/'), "{written}");
        assert!(!written.contains("redacted"), "{written}");
    }

    #[test]
    fn failed_connect_error_line_names_the_target_address() {
        let log = LogDir::new();
        let target = Ipv4Addr::new(127, 0, 0, 1);
        let err = match CastLink::connect(target, 1) {
            Ok(_) => panic!("port 1 must not accept a cast connection"),
            Err(err) => err,
        };
        let named = format!("TV unreachable at {target}");
        assert!(err.contains(&named), "{err}");
        let written = log.text();
        let line = written
            .lines()
            .find(|l| l.contains(" error "))
            .unwrap_or("");
        assert!(line.contains(&named), "{written}");
        assert!(line.contains(&format!("host={target}")), "{written}");
    }

    #[test]
    fn starting_a_cast_with_no_prior_session_writes_no_disconnect_line() {
        let log = LogDir::new();
        let snap = snapshot_ifaces();
        let iface = snap
            .bind
            .clone()
            .or_else(|| snap.preferred.first().cloned())
            .expect("a home-LAN address is required to start a cast");
        let state = CastState::default();
        let err = state
            .connect(&iface.ip.to_string(), Some(1), None)
            .expect_err("port 1 is not a cast receiver");
        assert_ne!(err, NO_HOME_LAN_MESSAGE, "{err}");
        let written = log.text();
        assert!(
            !written.contains(" disconnect "),
            "clearing an empty session must not log disconnect: {written}"
        );
        assert_eq!(state.status().error.as_deref(), Some(err.as_str()));
        state.disconnect().unwrap();
        let written = log.text();
        assert!(
            !written.contains(" disconnect "),
            "a user Disconnect with nothing connected must not log: {written}"
        );
        assert_eq!(state.status().error.as_deref(), Some(err.as_str()));
    }

    #[test]
    fn session_ready_timeout_logs_source_timeout_and_sets_status_error() {
        assert!(CastOrigin::parse("timeout").is_err());
        assert_eq!(CastOrigin::Timeout.as_str(), "timeout");
        let log = LogDir::new();
        let snap = snapshot_ifaces();
        let iface = snap
            .bind
            .clone()
            .or_else(|| snap.preferred.first().cloned())
            .expect("a home-LAN address is required to start a cast");
        let listener = TcpListener::bind((iface.ip, 0)).expect("bind stalling peer");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        thread::spawn(move || {
            listener.set_nonblocking(true).ok();
            while !stop_t.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        // Keep the TCP session open and never answer TLS.
                        sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
                        let mut buf = [0u8; 1024];
                        while !stop_t.load(Ordering::SeqCst) {
                            match sock.read(&mut buf) {
                                Ok(0) => break,
                                Ok(_) => {}
                                Err(err)
                                    if err.kind() == ErrorKind::WouldBlock
                                        || err.kind() == ErrorKind::TimedOut => {}
                                Err(_) => break,
                            }
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        let state = CastState::default();
        let started = Instant::now();
        let err = state
            .connect_wait(
                &iface.ip.to_string(),
                Some(port),
                None,
                Duration::from_millis(400),
            )
            .expect_err("a peer that accepts and stays silent must time out");
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        assert!(
            elapsed >= Duration::from_millis(350) && elapsed < Duration::from_secs(6),
            "ready wait should be ~400ms plus the TLS join, got {elapsed:?}"
        );
        assert!(err.contains("TV unreachable"), "{err}");
        assert_eq!(state.status().error.as_deref(), Some(err.as_str()));
        let written = log.text();
        let lines: Vec<_> = written
            .lines()
            .filter(|l| l.contains(" disconnect "))
            .collect();
        assert_eq!(lines.len(), 1, "{written}");
        assert!(lines[0].contains("source=timeout"), "{}", lines[0]);
        assert!(!lines[0].contains("source=user"), "{}", lines[0]);
        state.disconnect().unwrap();
        let after = log.text();
        let lines: Vec<_> = after
            .lines()
            .filter(|l| l.contains(" disconnect "))
            .collect();
        assert_eq!(lines.len(), 1, "{after}");
    }

    #[test]
    fn finished_video_loads_the_next_photo_with_no_user_input() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            idle_after: &[(2, "FINISHED", 7)],
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let photo1 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-1";
        let video = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/video-2";
        let photo3 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-3";
        let (_remote, mut flags) = session_after_load("still");
        let sid = link
            .load_url(photo1, "image/jpeg", true, CastOrigin::User, 1, "photo")
            .unwrap();
        flags.media_session = Some(sid);
        flags.media_kind = "still".into();
        flags.item_index = 1;
        let sid = link
            .load_url(video, "video/mp4", true, CastOrigin::Timer, 2, "video")
            .unwrap();
        flags.media_session = Some(sid);
        flags.advanced_session = None;
        flags.media_kind = "video".into();
        flags.item_index = 2;
        let mut third = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !third {
            match link.pump() {
                Ok(Some(update)) => {
                    if update.player == Some(PlayerState::Idle) {
                        on_media_idle(
                            &link,
                            &mut flags,
                            update.media_session,
                            update.idle_reason,
                            Ok(Successor {
                                url: photo3.to_string(),
                                content_type: "image/jpeg".into(),
                                item: 3,
                                media_type: "photo",
                            }),
                        )
                        .unwrap();
                        third = flags.item_index == 3;
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        assert!(third, "FINISHED must load the third item");
        let text = fake.trace_text();
        let loads: Vec<_> = text.lines().filter(|l| l.starts_with("load ")).collect();
        assert_eq!(loads, [
            "load transport-1 item=photo-1",
            "load transport-1 item=video-2",
            "load transport-1 item=photo-3",
        ], "{text}");
        let idle_at = text.find("idle FINISHED session=7").expect(&text);
        let third_at = text.rfind("item=photo-3").expect(&text);
        assert!(idle_at < third_at, "{text}");
        assert!(!text.contains("PAUSE") && !text.contains("STOP"), "{text}");
        let written = log.text();
        let advance = written
            .lines()
            .find(|l| l.contains(" advance "))
            .expect(&written);
        assert!(advance.contains("source=remote"), "{advance}");
        assert!(!advance.contains('/') && !advance.contains('\\'), "{advance}");
        let remote_loads: Vec<_> = written
            .lines()
            .filter(|l| l.contains(" load ") && l.contains("source=remote"))
            .collect();
        assert_eq!(remote_loads.len(), 1, "{written}");
        assert!(
            remote_loads[0].contains("item=3 type=photo"),
            "{}",
            remote_loads[0]
        );
        assert_eq!(
            written.lines().filter(|l| l.contains(" advance ")).count(),
            1,
            "{written}"
        );
        assert!(written.contains("item=1 type=photo"), "{written}");
        assert!(written.contains("item=2 type=video"), "{written}");
    }

    #[test]
    fn idle_from_stop_or_error_does_not_advance() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            // CANCELLED is the receiver's idleReason for a STOP. STOP is not a
            // finished playback, and neither is ERROR or a FINISHED for another item.
            idle_after: &[(1, "ERROR", 7), (1, "CANCELLED", 7), (1, "STOP", 7), (1, "FINISHED", 99)],
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let video = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/video-2";
        let photo3 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-3";
        let (_remote, mut flags) = session_after_load("video");
        let sid = link
            .load_url(video, "video/mp4", true, CastOrigin::User, 2, "video")
            .unwrap();
        flags.media_session = Some(sid);
        flags.advanced_session = None;
        flags.item_index = 2;
        let mut advanced = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match link.pump() {
                Ok(Some(update)) => {
                    if update.player == Some(PlayerState::Idle) {
                        let before = flags.item_index;
                        on_media_idle(
                            &link,
                            &mut flags,
                            update.media_session,
                            update.idle_reason,
                            Ok(Successor {
                                url: photo3.to_string(),
                                content_type: "image/jpeg".into(),
                                item: 3,
                                media_type: "photo",
                            }),
                        )
                        .unwrap();
                        if flags.item_index != before {
                            advanced = true;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        assert!(!advanced, "STOP, ERROR, and a stale FINISHED must not load the next item");
        let text = fake.trace_text();
        assert!(text.contains("idle ERROR session=7"), "{text}");
        assert!(text.contains("idle CANCELLED session=7"), "{text}");
        assert!(text.contains("idle STOP session=7"), "{text}");
        assert!(text.contains("idle FINISHED session=99"), "{text}");
        let loads: Vec<_> = text.lines().filter(|l| l.starts_with("load ")).collect();
        assert_eq!(loads, ["load transport-1 item=video-2"], "{text}");
        assert!(!log.text().contains(" advance "), "{}", log.text());
        assert!(
            log.text().contains("load_failed item=2 reason=ERROR"),
            "{}",
            log.text()
        );
    }

    #[test]
    fn photo_fetch_error_stops_without_loading_the_next_item() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            idle_after: &[(1, "ERROR", 7)],
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let photo1 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-1";
        let photo2 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-2";
        let (remote, mut flags) = session_after_load("still");
        let sid = link
            .load_url(photo1, "image/jpeg", true, CastOrigin::User, 1, "photo")
            .unwrap();
        flags.media_session = Some(sid);
        flags.item_index = 1;
        flags.media_kind = "still".into();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_error = false;
        while Instant::now() < deadline && !saw_error {
            match link.pump() {
                Ok(Some(update)) => {
                    if update.player == Some(PlayerState::Idle) {
                        on_media_idle(
                            &link,
                            &mut flags,
                            update.media_session,
                            update.idle_reason,
                            Ok(Successor {
                                url: photo2.to_string(),
                                content_type: "image/jpeg".into(),
                                item: 2,
                                media_type: "photo",
                            }),
                        )
                        .unwrap();
                        saw_error = flags.error.is_some();
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        assert!(saw_error, "a failed fetch must be shown to the user");
        let shown = flags.error.clone().unwrap_or_default();
        assert_eq!(shown, MEDIA_FETCH_MESSAGE);
        assert!(!shown.contains('/') && !shown.contains('\\'), "{shown}");
        assert_eq!(
            remote.lock().unwrap().as_deref(),
            Some("load_failed"),
            "the panel reads this event"
        );
        let text = fake.trace_text();
        let loads: Vec<_> = text.lines().filter(|l| l.starts_with("load ")).collect();
        assert_eq!(
            loads,
            ["load transport-1 item=photo-1"],
            "ERROR must not send the LOAD for item 2: {text}"
        );
        let written = log.text();
        assert!(written.contains("load_failed item=1 reason=ERROR"), "{written}");
        assert!(written.contains("item=1 type=photo"), "{written}");
        assert!(!written.contains(" advance "), "{written}");
        assert!(!written.contains("photo-2"), "{written}");
    }

    fn socket_loads(fake: &FakeTv) -> Vec<String> {
        fake.trace_text()
            .lines()
            .filter(|line| line.starts_with("load "))
            .map(|line| line.to_string())
            .collect()
    }

    #[test]
    fn photo_finished_does_not_load_the_next_item() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            idle_after: &[(1, "FINISHED", 7)],
            idle_delay: Duration::from_secs(10),
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let photo1 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-1";
        let photo2 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-2";
        let (remote, mut flags) = session_after_load("still");
        let sid = link
            .load_url(photo1, "image/jpeg", true, CastOrigin::User, 1, "photo")
            .unwrap();
        let started = Instant::now();
        flags.media_session = Some(sid);
        flags.media_kind = "still".into();
        flags.item_index = 1;
        flags.slideshow_playing = true;
        flags.advanced_session = None;
        flags.ignored_photo = None;
        let mut saw_finished = false;
        let mut finished_at = Duration::ZERO;
        let mut next_ping = Instant::now();
        let deadline = started + Duration::from_secs(20);
        while Instant::now() < deadline && !saw_finished {
            if Instant::now() >= next_ping {
                link.ping().expect("ping");
                next_ping = Instant::now() + PING_EVERY;
            }
            match link.pump() {
                Ok(Some(update)) => {
                    if update.player == Some(PlayerState::Idle) && !saw_finished {
                        saw_finished = true;
                        finished_at = started.elapsed();
                        assert_eq!(
                            socket_loads(&fake),
                            ["load transport-1 item=photo-1"],
                            "FINISHED must not load the next photo"
                        );
                        let did = on_media_idle(
                            &link,
                            &mut flags,
                            update.media_session,
                            update.idle_reason,
                            Ok(Successor {
                                url: photo2.to_string(),
                                content_type: "image/jpeg".into(),
                                item: 2,
                                media_type: "photo",
                            }),
                        )
                        .unwrap();
                        assert!(!did, "a photo FINISHED must not advance");
                        assert_eq!(flags.item_index, 1);
                        assert!(flags.error.is_none(), "{:?}", flags.error);
                        let ev = remote.lock().unwrap().clone();
                        assert_ne!(ev.as_deref(), Some("finished"));
                        assert_ne!(ev.as_deref(), Some("load_failed"));
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        assert!(saw_finished, "the fake TV never sent FINISHED");
        assert!(
            finished_at >= Duration::from_millis(8500)
                && finished_at <= Duration::from_millis(12500),
            "FINISHED arrived at {finished_at:?}"
        );
        assert_eq!(
            socket_loads(&fake),
            ["load transport-1 item=photo-1"],
            "a photo FINISHED does not load the next item"
        );
        let written = log.text();
        let ignored: Vec<_> = written
            .lines()
            .filter(|line| line.contains("photo_finished_ignored"))
            .collect();
        assert_eq!(ignored.len(), 1, "{written}");
        assert!(ignored[0].contains("item=1"), "{}", ignored[0]);
        assert!(!written.contains("source=timer"), "{written}");
        assert!(!written.contains(" advance "), "{written}");
        assert!(!written.contains("source=remote"), "{written}");
        assert!(!written.contains("load_failed"), "{written}");
        assert!(flags.error.is_none(), "{:?}", flags.error);
    }

    #[test]
    fn video_finished_advances_exactly_once() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            idle_after: &[(1, "FINISHED", 7)],
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let video = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/video-1";
        let photo2 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-2";
        let (_remote, mut flags) = session_after_load("video");
        flags.slideshow_playing = true;
        let sid = link
            .load_url(video, "video/mp4", true, CastOrigin::User, 1, "video")
            .unwrap();
        flags.media_session = Some(sid);
        flags.media_kind = "video".into();
        flags.item_index = 1;
        flags.advanced_session = None;
        let mut advances = 0u32;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match link.pump() {
                Ok(Some(update)) => {
                    if update.player == Some(PlayerState::Idle) {
                        let before = flags.item_index;
                        on_media_idle(
                            &link,
                            &mut flags,
                            update.media_session,
                            update.idle_reason,
                            Ok(Successor {
                                url: photo2.to_string(),
                                content_type: "image/jpeg".into(),
                                item: 2,
                                media_type: "photo",
                            }),
                        )
                        .unwrap();
                        if flags.item_index != before {
                            advances += 1;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        assert_eq!(advances, 1, "a video FINISHED advances once");
        assert_eq!(
            socket_loads(&fake),
            [
                "load transport-1 item=video-1",
                "load transport-1 item=photo-2",
            ],
        );
        let written = log.text();
        assert_eq!(
            written.lines().filter(|line| line.contains(" advance ")).count(),
            1,
            "{written}"
        );
        let remote_loads: Vec<_> = written
            .lines()
            .filter(|line| line.contains(" load ") && line.contains("source=remote"))
            .collect();
        assert_eq!(remote_loads.len(), 1, "{written}");
        assert!(remote_loads[0].contains("item=2 type=photo"), "{}", remote_loads[0]);
        assert!(!written.contains("photo_finished_ignored"), "{written}");
    }

    #[test]
    fn paused_slideshow_does_not_advance_on_photo_or_video_finished() {
        let log = LogDir::new();
        let fake = spawn_fake(FakeScript {
            idle_after: &[(1, "FINISHED", 7), (2, "FINISHED", 7)],
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let photo1 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-1";
        let video = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/video-2";
        let photo3 = "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/photo-3";
        let (remote, mut flags) = session_after_load("still");
        flags.slideshow_playing = false;
        let sid = link
            .load_url(photo1, "image/jpeg", true, CastOrigin::User, 1, "photo")
            .unwrap();
        flags.media_session = Some(sid);
        flags.media_kind = "still".into();
        flags.item_index = 1;
        flags.slideshow_playing = false;
        let next_photo = Successor {
            url: photo3.to_string(),
            content_type: "image/jpeg".into(),
            item: 2,
            media_type: "photo",
        };
        let mut saw_photo = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !saw_photo {
            match link.pump() {
                Ok(Some(update)) => {
                    if update.player == Some(PlayerState::Idle) {
                        let did = on_media_idle(
                            &link,
                            &mut flags,
                            update.media_session,
                            update.idle_reason,
                            Ok(Successor {
                                url: next_photo.url.clone(),
                                content_type: next_photo.content_type.clone(),
                                item: next_photo.item,
                                media_type: next_photo.media_type,
                            }),
                        )
                        .unwrap();
                        assert!(!did);
                        saw_photo = true;
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        assert!(saw_photo, "the photo FINISHED never arrived");
        assert_eq!(flags.item_index, 1);
        assert_eq!(socket_loads(&fake), ["load transport-1 item=photo-1"]);

        let sid = link
            .load_url(video, "video/mp4", false, CastOrigin::User, 2, "video")
            .unwrap();
        flags.media_session = Some(sid);
        flags.media_kind = "video".into();
        flags.item_index = 2;
        flags.slideshow_playing = false;
        flags.advanced_session = None;
        flags.error = None;
        let mut saw_video = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !saw_video {
            match link.pump() {
                Ok(Some(update)) => {
                    if update.player == Some(PlayerState::Idle) {
                        let did = on_media_idle(
                            &link,
                            &mut flags,
                            update.media_session,
                            update.idle_reason,
                            Ok(Successor {
                                url: photo3.to_string(),
                                content_type: "image/jpeg".into(),
                                item: 3,
                                media_type: "photo",
                            }),
                        )
                        .unwrap();
                        assert!(!did, "a paused video FINISHED must not advance");
                        saw_video = true;
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("pump: {e}"),
            }
        }
        assert!(saw_video, "the video FINISHED never arrived");
        assert_eq!(flags.item_index, 2);
        assert!(flags.error.is_none(), "{:?}", flags.error);
        assert_ne!(remote.lock().unwrap().as_deref(), Some("finished"));
        assert_eq!(
            socket_loads(&fake),
            [
                "load transport-1 item=photo-1",
                "load transport-1 item=video-2",
            ],
        );
        let written = log.text();
        assert!(written.contains("photo_finished_ignored item=1"), "{written}");
        assert!(!written.contains(" advance "), "{written}");
        assert!(!written.contains("source=remote"), "{written}");
        assert!(!written.contains("load_failed"), "{written}");
    }

    #[test]
    fn a_20s_timer_load_logs_that_duration_and_the_next_photo_can_change_it() {
        assert_eq!(photo_timer_ms(Some(20_000)), 20_000);
        assert_eq!(photo_timer_ms(None), 10_000);
        assert_eq!(photo_timer_ms(Some(0)), 10_000);
        assert_eq!(DEFAULT_PHOTO_TIMER_MS, 10_000);

        let log = LogDir::new();
        let path = scratch("hold.jpg", &jpeg_bytes());
        let mut http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        http.set_playlist(std::slice::from_ref(&path)).unwrap();
        let fake = spawn_fake(FakeScript {
            still: true,
            ..FakeScript::default()
        });
        let (ip, port) = fake.host_port();
        let mut link = CastLink::connect(ip, port).unwrap();
        link.launch_receiver().unwrap();
        let status = Mutex::new(CastLiveStatus {
            connected: true,
            player_state: "idle".into(),
            remote_event: None,
            error: None,
            media_kind: String::new(),
            http_port: http.port,
            bind_ip: http.bind_ip.to_string(),
            item_index: 0,
        });
        let (_remote, mut flags) = session_after_load("still");
        flags.media_kind.clear();
        flags.media_session = None;
        flags.slideshow_playing = true;
        let mut running = true;

        let load_at = |http: &mut MediaHttp,
                       link: &CastLink,
                       flags: &mut SessionFlags,
                       status: &Mutex<CastLiveStatus>,
                       running: &mut bool,
                       duration_ms: Option<u64>| {
            let (tx, rx) = mpsc::channel();
            let kept = handle_cmd(
                CastCmd::Load {
                    path: path.clone(),
                    autoplay: true,
                    origin: CastOrigin::Timer,
                    duration_ms,
                    reply: tx,
                },
                http,
                link,
                flags,
                status,
                running,
            );
            assert!(kept);
            assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().is_ok());
        };
        load_at(
            &mut http,
            &link,
            &mut flags,
            &status,
            &mut running,
            Some(20_000),
        );
        load_at(
            &mut http,
            &link,
            &mut flags,
            &status,
            &mut running,
            Some(8_000),
        );

        let written = log.text();
        let timer_loads: Vec<_> = written
            .lines()
            .filter(|line| line.contains(" load ") && line.contains("source=timer"))
            .collect();
        assert_eq!(timer_loads.len(), 2, "{written}");
        assert!(
            timer_loads[0].contains("dur_ms=20000"),
            "first timer used the 20s session duration: {}",
            timer_loads[0]
        );
        assert!(
            !timer_loads[0].contains("dur_ms=10000"),
            "the 10s default must not replace 20s: {}",
            timer_loads[0]
        );
        assert!(
            timer_loads[1].contains("dur_ms=8000"),
            "the next photo uses the duration changed while casting: {}",
            timer_loads[1]
        );
        assert!(!written.contains(" play "), "{written}");
    }

    #[test]
    fn successor_is_the_next_playlist_item() {
        let first = scratch("one.jpg", &jpeg_bytes());
        let skipped = scratch("skip.webp", b"RIFF");
        let second = scratch("two.mp4", &mp4_with(&[b"avc1", b"mp4a"]));
        let http = MediaHttp::bind_on(Ipv4Addr::LOCALHOST, std::iter::once(0)).unwrap();
        let update = http
            .set_playlist(&[first.clone(), skipped.clone(), second.clone()])
            .unwrap();
        assert_eq!(update.registered, 2);
        assert_eq!(update.page_index, vec![1, 3]);
        assert_eq!(http.index_of(&first).unwrap(), 1);
        assert_eq!(http.index_of(&second).unwrap(), 3);
        assert!(http.index_of(&skipped).is_err());
        let next = http.successor(1).unwrap();
        assert_eq!(next.item, 3);
        assert_eq!(next.media_type, "video");
        assert!(next.url.starts_with(&format!("http://127.0.0.1:{}", http.port)));
        let wrapped = http.successor(3).unwrap();
        assert_eq!(wrapped.item, 1);
        assert_eq!(wrapped.media_type, "photo");
        match http.successor(0) {
            Err(err) => assert_eq!(err, "current item unknown"),
            Ok(_) => panic!("item 0 is not a playlist index"),
        }
        let unknown = load_log_detail(
            "192.168.1.52:47200",
            2,
            "user",
            0,
            "photo",
            "http://192.168.1.52:47200/m/0123456789abcdef0123456789abcdef/9",
            "image/jpeg",
            "reply=7",
        );
        assert!(unknown.contains("item=?"), "{unknown}");
        assert!(
            unknown.contains("url=http://192.168.1.52:47200/m/9?t=***"),
            "{unknown}"
        );
        assert!(unknown.contains("contentType=image/jpeg"), "{unknown}");
        assert!(!unknown.contains("0123456789abcdef0123456789abcdef"), "{unknown}");
        assert!(!unknown.contains('\\'), "{unknown}");
    }

    #[test]
    fn media_failure_is_visible_on_cast_status_without_pretending_to_play() {
        let status = Mutex::new(CastLiveStatus {
            connected: true,
            player_state: "playing".into(),
            remote_event: None,
            error: None,
            media_kind: "still".into(),
            http_port: 47200,
            bind_ip: "192.168.1.49".into(),
            item_index: 1,
        });
        let (_remote, mut flags) = session_after_load("still");
        flags.player_state = "playing".into();
        flags.connected = true;
        publish_media_failure(&mut flags, &status, MEDIA_FETCH_MESSAGE, false);
        let live = status.lock().unwrap().clone();
        assert_eq!(live.error.as_deref(), Some(MEDIA_FETCH_MESSAGE));
        assert_ne!(live.player_state, "playing");
        assert!(live.connected, "a media timeout does not tear the session down");
        publish_media_failure(&mut flags, &status, &link_down_message(), true);
        let live = status.lock().unwrap().clone();
        assert!(!live.connected);
        assert!(live.error.as_deref().unwrap_or("").contains("TV unreachable"));
        assert_ne!(live.player_state, "playing");
    }
}
