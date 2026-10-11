# SlideX Cast (0.1.8)

Cast sends the open slideshow to a Google TV / Chromecast on the home network. It is content cast through the Default Media Receiver, not AirPlay, DLNA, desktop mirroring, or a cloud relay.

This release stacks on the 0.1.7 display rename (`cursor/slidex-0-1-7-display-2c94`, PR #34). The install identity is unchanged: `com.johnalindogan.slideshowpro`, `slideshowpro.exe`, uninstall key `Software\Microsoft\Windows\CurrentVersion\Uninstall\SlideShowX`.

## What was actually wrong

- The Cast spike script started with `(/* Cast spike UI */)();`, which is a syntax error, so the panel never ran. The panel HTML was also after that script, and the script called `invoke`, which is not in scope. The global helper is `tauriInvoke`.
- `cast_load_still` / `cast_load_video` ignored the playlist and served only the two files under `demo/cast-spike/`. The HTTP server listened on `0.0.0.0` and read each file into memory.
- Discovery treated mdns-sd's idle wait as a failure. flume 0.11 prints that wait as `timed out waiting on a channel`, which does not contain the substring `timeout`. A quiet poll was stored and shown as `mdns: timed out waiting on a channel` whenever the device list was empty.
- The spike also called `disable_interface(IfKind::All)` and ignored the error from enabling the LAN address back. If that enable failed, browse listened on no interfaces.
- On John's network the PLDT router had Wi-Fi client isolation on for both bands. That alone produces an empty browse and the same "no devices" symptom. Isolation is now off. The no-TV error names it.
- After that network change, Chrome kept a stale "No devices found" until it was restarted. SlideX does not keep a device cache. Each browse starts empty, the list is replaced only by that browse, and a change of home-LAN adapters starts a new browse. The panel also has Rescan. An empty result is scanned again while the panel stays open, so fixing the router does not require restarting SlideX.

## Stack

`mdns-sd` browse of `_googlecast._tcp.local.`, `rust_cast` 0.21 to the Default Media Receiver, and a small HTTP server bound to one home-LAN IPv4 address. The PC slideshow clock (slide duration and video end) is what advances the TV. Play, pause, next, and previous on the PC call the Cast session. A pause from the TV remote is applied back to the PC player.

## Discovery

- Browse is capped at 10 seconds (the button uses 8). The command returns in that window, so the panel can show the no-TV error well under 15 seconds.
- 2.4 GHz and 5 GHz on one router are one LAN. A receiver is kept when its IPv4 is in the subnet of any home-LAN adapter. The radio is not a filter. Manual IP accepts any home-network IPv4 (for example `192.168.1.50`) and defaults the port to 8009. Public, CGNAT, and link-local addresses are rejected.
- Idle mDNS waits are not errors. A closed channel is.
- There is no saved device list. `cast_network_fingerprint` is the current home-LAN adapters. When it changes, SlideX starts a new browse and drops the previous list first. Rescan does the same.

Plain error when nothing answers:

> No TV found on this network. Check that the PC and TV are on the same Wi-Fi, the firewall, or the router's client isolation.

If Windows has marked the home-LAN adapter Public, discovery, a cast that never starts, and a photo or video that does not load within 15 seconds all show this instead:

> This Wi-Fi is set to Public in Windows. Set it to Private to cast.

The category comes from `Get-NetConnectionProfile` on the adapter SlideX bound (the interface alias, not the SSID). Private and Domain keep the scenario message. If the category cannot be read, it is treated as unknown and the scenario message stays. SlideX does not change the category.

When the TV is found but the slide does not start within 15 seconds, and the network is Private or the category cannot be read:

> The TV was found, but the photo or video did not load within 15 seconds.

The status line also names the adapters that were browsed and, when the PC has no home-LAN address, that VPN or Tailscale may be the only route. The slideshow keeps working, and Cast can be tried again without restarting.

## Media server

- Binds the physical Wi-Fi or Ethernet adapter whose subnet contains the TV. NordLynx, WireGuard, Hyper-V `vEthernet`, WSL, and other virtual or VPN adapters are skipped even when they use an RFC1918 address. If none of those physical adapters is on the TV's subnet, Cast shows the VPN or Tailscale message. Never `0.0.0.0`, a public address, or Tailscale.
- Port is chosen at random inside TCP **47200–47215** so the firewall rule can name that range. UDP 5353 is discovery only.
- Each URL is `http://<lan-ip>:<port>/m/<session-token>/<file-key>`. The token is per session. The key is random per playlist file. The frontend is not given the token.
- Only files in the current playlist are registered. Directory listing, `..`, encoded dots, extra path segments, and a wrong token are 404. The book is replaced when the playlist changes and revoked when casting stops. The listener closes on disconnect and when the app exits.
- Each connection is its own thread, up to 8 at once. Past that the server answers 503 and does not queue. Sockets are non-blocking. A full TV buffer returns `WouldBlock` and the thread waits about 8 ms, then writes again only the bytes that were not accepted. It does not use a socket timeout, because on Windows that leaves the socket undefined and a retried write can drop or repeat bytes. Every 64 KB it checks the stop flag and that the session token is still valid. A connection with no forward progress for 30 seconds is closed so it cannot keep one of the 8 slots. Stop or a revoke closes the socket, so a video range cannot hold the next photo or a shutdown.
- Nothing is logged that contains a token, a device UUID, or a full path.
- After the token check, `transform_media` opens the allowlisted file. That is the identity transform. A later slideshow-only crop sidecar can return a generated still from that function without changing auth or the socket loop. Crop is not implemented in this release.

JPEG, PNG, and MP4 (H.264 + AAC) are registered. Anything else is skipped with a note and the slideshow moves on.

## Receiver handshake

After TLS, the sender connects the virtual receiver (`CONNECT` to `receiver-0`), then `LAUNCH` for the Default Media Receiver (`CC1AD845`) and waits for the `RECEIVER_STATUS` with the same `requestId`. It then connects that `transportId` and sends `LOAD` on the media namespace to the transport, not to `receiver-0`. `PING` is answered with `PONG`. The sender also pings every 5 seconds. `LAUNCH_ERROR` and `LOAD_FAILED` are shown immediately. A launch that never answers, including Windows error 10060, says: "The TV didn't answer the launch request. Make sure it's on and try again."

Each step is appended to `%LOCALAPPDATA%\com.johnalindogan.slideshowpro\logs\cast.log` (rotated at 1 MB). The load line records the media URL host and port so a log can show the home LAN address rather than a Tailscale address. The line does not include the media token or a file path. The Cast panel has "Copy Cast log path".

## Playback and failure

The TV follows the PC order, including reverse playback, and the PC slide timer. Controls are sent on the Cast channel; the idle read is 350 ms, so a healthy LAN should land play, pause, next, and previous within a second. A TV-remote pause or play comes back as `remote_event` and the PC player follows it.

If the TV is off or Wi-Fi drops, the session reports after about 15 seconds of silence (the panel polls at 400 ms):

> TV unreachable. It may be off or the Wi-Fi dropped. Cast again when it is back — you do not need to restart SlideX.

Connecting again starts a new session and a new token. The app does not need a restart.

## Firewall

The NSIS install is per-user (`RequestExecutionLevel user`, files under `%LOCALAPPDATA%\SlideShowX`). Windows will not add a port-scoped Private-only rule from that context. The elevated step is still one `ExecShell "runas"` of `cmd.exe /c` with the `netsh advfirewall` commands as arguments, not a script. `cmd /c` strips only the first and last quote, so rule names and paths use plain inner quotes (`name="SlideX Cast media (Private)"`), not backslash-quotes. `cast-fw-check.ps1` is one entry in the single `bundle.resources` map, with target name `cast-fw-check.ps1`. The installer's resources `File` loop copies it to `$INSTDIR\cast-fw-check.ps1` beside `slideshowpro.exe` before the prompt, and the uninstall loop deletes it with the other resources. The check runs unelevated:

`$SYSDIR\WindowsPowerShell\v1.0\powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$INSTDIR\cast-fw-check.ps1" -ExePath "$INSTDIR\slideshowpro.exe"`

`-ExecutionPolicy Bypass` is process-scoped. The script calls `Get-NetFirewallRule` and the application, address, and port filters. The exe path is a parameter, so a folder name like `O'Brien` is not interpolated into quotes. If both rules already match this `slideshowpro.exe` (enabled, inbound allow, Private, `LocalSubnet`, TCP 47200–47215 and UDP 5353), it prints `status=match`, the installer skips the prompt, and it writes `status=present`. Uninstall deletes `cast-fw-check.ps1`. The rules are:

| Rule | Program | Profile | Ports | Remote |
|---|---|---|---|---|
| SlideX Cast media (Private) | `slideshowpro.exe` | Private | TCP 47200–47215 | `localsubnet` |
| SlideX Cast mDNS (Private) | `slideshowpro.exe` | Private | UDP 5353 | `localsubnet` |

Public is never set. The only file written is `$INSTDIR\cast-firewall.txt`. The elevated command prints `status=added` or `status=failed` there, and the installer copies that file into the detail log. If the prompt is declined or netsh fails, the detail log says so and a message box says to re-run the installer. Leftover `cast-firewall-add.cmd` and `cast-firewall.ok` files are deleted and not recreated. The Cast panel shows the same fact from `cast_firewall_status`, which runs the same script with `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File`. If `cast-fw-check.ps1` is missing beside the app, the panel and the app log say that the firewall check script is not installed. That line does not include a path outside the install directory or a device id. A Public-only rule is not counted as installed. Because the rules are Private-only, a Public network profile cannot discover or serve; the panel says to set the Wi-Fi to Private.

Uninstall removes both rules with one elevated `cmd.exe /c` (inline `netsh` deletes, no script), unless the uninstall is the upgrade handoff (`/UPDATE`). An upgrade also skips the prompt when the rules already match this exe. This PR does not build or publish the NSIS installer. QA sees the elevated command in the detail log, `cast-firewall.txt` next to `slideshowpro.exe`, and the Cast panel line.

A rule without an admin prompt cannot be limited to those ports and to Private only. The Windows "allow this app" prompt follows the current network profile and is not port-scoped, so the installer uses the UAC step instead.

## Homelab live checklist

Run this on ZB23 against the real Google TV. Do not treat a unit test as this list. This environment cannot reach the TV.

John's network, after client isolation was turned off on the PLDT router:

- ZB23 is on the 5 GHz SSID, Windows firewall profile Private.
- The TV is on the 2.4 GHz SSID of the same router. It advertises `fn="Living room TV"`, `md=Chromecast`, at `192.168.1.56`. Ping works and TCP 8009 is open. mDNS answers. Those facts are the live target only. The app does not hardcode them.
- Manual IP fallback should accept `192.168.1.56` (port 8009 if left blank).

Checklist:

1. Discover finds the TV within 10 seconds, 5 times out of 5, including across the 2.4 and 5 GHz bands.
2. With the TV unreachable, the panel shows the plain no-TV error (same Wi-Fi, firewall, router client isolation) within 15 seconds, and the slideshow still works. If Windows has this Wi-Fi set to Public, the panel shows "This Wi-Fi is set to Public in Windows. Set it to Private to cast." instead, including when the TV is found but a photo or video does not load within 15 seconds.
3. Rescan starts a fresh browse. Changing the PC's network (or toggling Wi-Fi) starts a new browse without restarting SlideX. An empty list does not stick the way Chrome's did.
4. A 20-item mixed playlist (JPEG, PNG, MP4 H.264+AAC, plus at least one unsupported file) plays on the TV in the same order and with the same slide timing as the PC. The unsupported file is skipped with a visible note and does not stall the playlist.
5. Play, pause, next, and previous on the PC reach the TV within 1 second. A pause from the TV remote shows as paused in SlideX.
6. Disconnect closes the media server. Turning the TV off, or dropping Wi-Fi, shows a clear status after about 15 seconds of silence. Nothing hangs. Casting again works without restarting SlideX. Do one TV off/on cycle. The Cast log under `%LOCALAPPDATA%\com.johnalindogan.slideshowpro\logs\cast.log` shows the media host as the LAN address.
7. Leave a cast running for 30 minutes. No drops, and no script errors in the console.
8. After a real install, `cast-firewall.txt` and the Cast panel show Private-only rules for `slideshowpro.exe`: TCP 47200–47215 and UDP 5353. Uninstall removes them. Public is absent.
