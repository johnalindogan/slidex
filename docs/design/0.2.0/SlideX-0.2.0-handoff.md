# SlideX 0.2.0 — UX handoff

Auto Music Video, beat-synced music, Export music video, rebuilt second window, and a cleanup of the current screens.

**Deliverables**

| What | Where |
|---|---|
| Design canvas (22 artboards, timeline editor is interactive in Play) | SlideX 0.2.0 UX canvas in Claude |
| Plain HTML mockups, one self-contained page per screen + `index.html` | `SlideX-0.2.0-mockups.zip` |
| This handoff | `SlideX-0.2.0-handoff.md` |

Mockups use only plain HTML + CSS (+ a little vanilla JS on the canvas). No frameworks, CDNs or external fonts. Values marked *(proposed)* in your brief are kept easy to change.

---

## 1. Screen index

| # | Mockup file | Covers |
|---|---|---|
| 1 | `Main.html` | Landing (revised) |
| 2 | `Controls.html` | Bottom bar + Tools sheet (replaces More tools) |
| 3 | `Settings.html` | One Settings modal |
| 4 | `Cast.html` | Unified Cast panel + connecting / failed / casting file / beat-sync warning states |
| 5 | `MediaMgr.html` | Media Manager with labelled buttons, safe right-click menu, Recycle Bin confirm |
| 6 | `AmvMedia.html` | Wizard 1 · Pick media (+ too-few warning) |
| 7 | `AmvSong.html` | Wizard 2 · Pick a song |
| 8 | `AmvOptions.html` | Wizard 3 · Options (+ over 10 min error) |
| 9 | `AmvAnalyze.html` | Wizard 4 · Analyze (slow share, cached fast path) |
| 10 | `AmvTimeline.html` | Wizard 5 · Preview + edit timeline, Undo/Redo |
| 11 | `AmvGridFix.html` | Tempo unclear + Beat grid fix |
| 12 | `AmvRender.html` | Wizard 6 · Render (+ "Keep working" pill and Done toast) |
| 13 | `AmvDone.html` | Wizard 7 · Done |
| 14 | `AmvErrors.html` | Shared empty and error states |
| 15 | `Music.html` | Music tab + beat sync settings |
| 16 | `Export.html` | Export as music video (+ no music, over cap) |
| 17 | `Undock.html` | Second window, linked |
| 18 | `UndockStates.html` | Second window: opening, main busy, link dropped, after Dock back |
| 19–22 | `Audit.html`, `CopyDeck.html`, `Porting.html`, `Ideas.html` | Same content as sections 3–7 below |

---

## 2. Key decisions

- **Cast panel: unify it.** Use the glass tokens. Keep the blue `#9fd0ff` only as the app-wide **live** accent (connected, casting, analyzed), always shown with a dot **and** a word.
- **Encoders in plain words.** Auto · Compatible (CPU) · Windows built-in · NVIDIA graphics · Intel graphics. Show the raw name (`h264_nvenc` etc.) in small mono below. Option values stay `libx264`, `h264_mf`, `h264_nvenc`, `h264_qsv`.
- **More tools → Tools sheet.** Four labelled groups: Playback · Image · Motion & view · Export & create. It has type-to-find, and every row shows its key.
- **One Settings modal (Ctrl+,).** Sections: Playback, Ken Burns, Music & beat sync, Music video, Cast, Second window, Keyboard shortcuts, Advanced. The Ken Burns modal and shortcut editor become sections. "Image Updates" moves to Advanced.
- **Music lives in a Music tab** beside Media in the Media Manager. The bottom bar gets one "now playing" button (`#bmusic`).
- **Wizard:** a near-full-window modal with a 7-step stepper. The project autosaves. Esc closes, keeping the project, and Enter goes to the next step.
- **Second window:** the main window owns all state. The second window shows a skeleton until the first update arrives, keeps a link chip visible, and pauses editing if the link drops.
- **Undo/Redo in the timeline editor**, covering every change to the plan.
- **Contrast:** readable text at least 60% white. Keep 30% for decoration and disabled controls only.
- **Fonts:** bundle the font files locally (see §5).

---

## 3. UX audit (current 0.1.13)

**P0** loses data or breaks a core task · **P1** slows you down daily · **P2** polish.

| ID | Screen | Problem | Why it hurts | Fix |
|---|---|---|---|---|
| P0-1 | Media Manager right-click | "Delete file" sits beside Duplicate/Remove with the same weight | One slip deletes the original; on a network share there's no Recycle Bin | Divider + red "Move file to Recycle Bin…" on Shift+Del; confirm names the files, warns about shares and other playlists, offers "Remove from playlist only", focus starts on Cancel |
| P0-2 | Undocked window | Opened black; order wrong after Dock; no link state | The second screen shows nothing, or the wrong list | Main owns state; skeleton until the first snapshot; link chip; edits pause when unlinked; Dock confirms "order kept" |
| P0-3 | Cast panel | "Connecting…" with no time, step or cancel; a dead-end error | You can't tell whether to wait, retry or fix the firewall | Three named steps + timer + Cancel; 15 s timeout; plain error with firewall hint; Try again / Enter TV address; raw error + log path under Details |
| P1-1 | More tools | ~20 icon-only controls in one row, actions mixed with settings | You hunt by hovering; similar icons get mixed up | Tools sheet (above); bar keeps ~12 main controls |
| P1-2 | Settings | Split across 4 places | No single place; 0.2.0 settings have no home | One Settings modal |
| P1-3 | Media Manager header | Emoji icon buttons, two different folder emoji | Fonts render emoji differently; unreadable at a glance | Short text buttons with tooltips and keys, grouped Add · Organize · Window |
| P1-4 | Undock/Dock | "Undock" is jargon; the toast doesn't say where it went | The panel seems to vanish onto another monitor | "Open in window" / "Dock back", same key both ways (Ctrl+Shift+U) |
| P1-5 | Cast style | Separate palette and font; raw encoder names | Looks like another app; names mean nothing | Unify; plain encoder labels |
| P1-6 | Landing | Blue Cast tile reads as the default; Continue isn't emphasised; "Image Updates" unclear | The eye goes to the wrong place | Continue is the default (Enter); recent playlists inline; Make music video tile; Cast/Settings/Shortcuts in a quiet row |
| P1-7 | Shortcuts | Only discoverable in the editor | Bad for a keyboard-driven app | Keys in every tooltip and the Tools sheet; `?` cheat sheet |
| P1-8 | All | Text at .3 / .45 on dark fails 4.5:1 (≈2.4:1 / 4.4:1) | Hard to read on a dim monitor or TV | ≥ .6 for any readable text |
| P1-9 | Modals | No visible focus ring | Breaks keyboard use | 2 px white `:focus-visible`; focus trap; Esc closes the top layer only; Enter runs the primary button |
| P1-10 | Media Manager edits | Remove, Shuffle and reorder can't be undone | One stray Shuffle scrambles a curated order | Ctrl+Z / Ctrl+Y for playlist edits; Undo button on the Remove/Shuffle toast. File delete stays on the Recycle Bin |
| P2-1 | Playlist library | `10/24/2026, 6:00:00 PM`; Delete as loud as Load | Noise; destructive button draws the eye | `Oct 24, 6:00 PM`; Delete quieter (still confirmed) |
| P2-2 | Toast | Red action buttons on neutral messages | Red should mean destructive | Neutral by default |
| P2-3 | Ken Burns | "400%" has no reference; long per-slide list | Hard to predict | Label the slider ends Subtle/Strong; filter field |
| P2-4 | Bottom bar | `1/7` cramped; seek unlabeled | Readability, accessibility | `3 / 7`; seek is a labelled slider |
| P2-5 | Fonts | Loaded from Google Fonts | Offline, the layout shifts | Bundle locally |
| P2-6 | Media Manager | "3 selected" doesn't say what actions affect | Unclear scope | Labels like "Remove 3", "Move 3 files to Recycle Bin…" |

---

## 4. Feature specs (as designed)

### A. Auto Music Video wizard (`#amv`)
1. **Media** — source: Current playlist · Selected items · A folder…; a tick grid; count chips; too-few warning.
2. **Song** — the chosen song card with waveform, cached chip, songs from the music playlist with status, and a drop zone. Local files only.
3. **Options** — Style (Chill/Balanced/Hype, with a description of the cut rate); Length (Whole · 30 s · 60 s · Custom, max 10:00 *(proposed)*); Shape 16:9 / 9:16 / 1:1 at 1080p30 *(proposed)*; Clip sound: Song only (default *(proposed)*) + a disabled "Keep clips' own sound quietly underneath · Coming later"; Save to (never next to originals); summary card.
4. **Analyze** — overall progress + ETA; song card (cached, BPM, sections bar); per-file list (done / current with progress / waiting / slow share); skipped-stretch count; Cancel analysis · Run in background (Esc) · Open editor. Cached fast path: "Ready in 1 s".
5. **Edit** — 16:9 preview (low-res, not rendered); inspector (lock, swap, length in beats + split/merge, start point for video **or** Ken Burns preset for a photo, transition into the shot); toolbar (Undo, Redo, history count, Regenerate, Style, Beat grid…); timeline (ruler, section energy lane, beat/bar ticks, shot lane sized in beats, playhead, lock badge, crossfade edge, video mark).
   - **Beat grid fix:** tempo field + ½× / 2×, shift ±10 ms, tap along for the first beat of the bar, fallback "cut every N s", Reset · Cancel · Apply and re-plan (keeps locks that fit). Undoable.
6. **Render** — % + "shot i of n", elapsed / left, a segment bar showing reused vs new shots (partial re-render), background note, Cancel render · Keep working (Esc). Then a pill in the controls bar + Windows taskbar progress; quitting asks first.
7. **Done** — poster, file / length / size / folder, Play in SlideX (Enter) · Open folder · Cast it…, Back to the editor · Make another, the licence line. Leave room after "Cast it…" for "Render & Cast" (0.2.x).

### B. Music + beat sync (`#musicPane`)
- List with status chips: `124 BPM` (analyzed) · Cached · Analyzing n% · Tempo unclear · Can't read.
- Transport: prev / play-pause / next, position, Shuffle, Repeat, volume.
- Beat sync: Change slides **Every N beats** or **Every bar**; Transitions "Stronger on loud parts"; Sync offset slider (ms, saved); Beat grid… (G).
- While casting: a warning banner says beat sync covers this PC only, with a link to Export music video.

### C. Export as music video (`#mvx`)
Summary (items, songs, pacing, length); Shape; Clip sound (same as the wizard); Quality/encoder (Auto + raw name); Save to; licence line; Export (Enter). States: no music, over the cap. Progress and Done reuse wizard steps 6–7.

### D. Second window (`#sw`)
Bar: link chip · main window status · "Showing" view switcher (Media Manager only for now; later full-screen preview and the 0.3.0 singer screen) · Dock back (Ctrl+Shift+U). Content: a generic queue list (`.q` rows, reusable for the karaoke queue) + timeline grid + folders. States: Opening (skeleton) · Main busy rendering (browse OK, edits queued) · Link dropped (last copy shown, edits paused, Reconnect / Dock back) · After Dock (toast "Order and selection kept").

---

## 5. Porting note

### Tokens
```css
:root{
  --bg:#000; --panel:#141414; --panel-2:#111; --panel-3:#1a1a1a;
  --glass:rgba(10,10,10,.72); --line:rgba(255,255,255,.08);
  --hover:rgba(255,255,255,.08);
  --t-hi:rgba(255,255,255,.85); --t-mid:rgba(255,255,255,.6);
  --t-decor:rgba(255,255,255,.3);   /* never for readable text */
  --btn-line:rgba(255,255,255,.18);
  --on-bg:rgba(255,255,255,.15); --on-line:rgba(255,255,255,.55);
  --danger:rgba(220,60,60,1); --warn:#e8b35a; --live:#9fd0ff;
  --r-btn:6px; --r-modal:12px; --focus:2px solid #fff;
}
```
Reusable classes from the mockups: `.btn` (`.on .pri .dng .ico .sm`), `.seg`, `.bn` (`.warn .err .live`), `.chip`, `.prog`, `.kbd`, `.steps`, `.q`.

### Fonts
Bundle `PlusJakartaSans[wght].woff2` and `JetBrainsMono-Regular.woff2` in `/fonts` with `@font-face` and `font-display:swap`. Remove the Google Fonts link and ship `OFL.txt`. Fallbacks: `system-ui, "Segoe UI"` and `ui-monospace, Consolas`.

### DOM
```
body
├ #landing            (+ #lbamv #lbsettings #lbrecent)
├ #viewer > #stage #fnov #cropbar
├ #ctrls              (+ #bmusic #amvPill #bsettings)
├ #ctrls-overflow     (Tools sheet, + #toolsFind)
├ #media-dock         (+ tabs Media | #musicTab, + #musicPane)
└ #layers             (one stack; Esc closes the top layer)
   ├ #settings ├ #amv ├ #mvx ├ #plib
   ├ #castPanel ├ #delConfirm └ #toast
```
Second window (role `playlist`): `#sw > #swBar + #swContent`, with one view mounted at a time.

### Existing elements
| ID | Change |
|---|---|
| `#lbcast` | Tile → small button in the bottom row |
| `#lbcont` | Default action (Enter), outlined |
| `#bkb` `#bked` | Off the bar; open from Tools / Settings / `k` / `?` |
| `#bplt` | Label "Media" |
| `#bmore` | Label "Tools", `aria-expanded`, key `/` |
| `#ctrls-overflow` | Same ID; children move into 4 groups; `#dsel #tsel #vskip` keep their IDs |
| `#kbmodal` `#keditor` | Inner nodes move into `#setPane-kb` / `#setPane-keys`; their own backdrops go |
| `#castPanel` | Restyle; add `#castSteps #castDetails`; `#castEnc` option text changes, values unchanged |
| `#media-dock` | Emoji buttons → text; Undock → "Open in window" |
| `#plib` | Dates via `Intl.DateTimeFormat`, no seconds |
| `#toast` | Neutral action buttons; `.danger` only when destructive |
| Context menu | Delete → `#delConfirm` |

### New IDs
- Settings: `#settings #setSearch #setPane-*`
- Wizard: `#amv #amvSteps #amvBody #amvNext #amvSrc #amvGrid #amvSong #amvStyle #amvLen #amvAspect #amvOut #amvAnSong #amvAnMedia #amvPreview #amvPlay #amvInspector #amvShots #amvUndo #amvRedo #amvToast #amvRender #amvGridFix #amvRenderBox #amvDone #amvPill`
- Music: `#bmusic #musicTab #musicPane #musicList #musicNow`
- Export: `#mvx #mvxSummary #mvxAspect #mvxEnc #mvxOut #mvxGo`
- Second window: `#sw #swBar #swStatus #swShowing #swDock #swContent #swQueue`
- Other: `#delConfirm #toolsFind #lbamv #lbsettings #lbrecent #bsettings`

### JS changes
- **Layer stack:** `openLayer(id)` / `closeTop()`, focus trap, return focus to the opener, Esc closes the top, Enter runs `[data-primary]`.
- **Key scopes:** while `#amv` is open, single letters belong to the wizard and global keys pause. One table maps scope → key → command so the shortcut editor can check conflicts per scope.
- **Wizard state:** a `step` enum + a project JSON `{id,name,media[],song,options,plan:{shots[]},locks,history,renders[]}` saved in app data, never next to sources.
- **Undo/redo:** every plan edit goes through `commit(label, nextPlan)`, which pushes the previous plan to `past` (cap 100) and clears `future`. Store whole snapshots, since plans are small. Keys: Ctrl+Z, Ctrl+Y / Ctrl+Shift+Z. Tooltips name the step. History is saved with the project. Selection, Analyze and Render are not steps. Playlist edits in the Media Manager use the same model.
- **Rust side:** commands `amv_analyze(project)`, `amv_render(plan)`, `amv_cancel(job)`; events `amv://progress {phase,file,i,n,eta}` and `render://progress {pct,shot,reused,eta}`. Cache key = path + size + mtime + analyzer version. Render per shot, then concatenate, so an edit only redoes changed shots.
- **Timeline:** shot `flex-grow` = beats; ticks are a CSS gradient on `--px-per-beat`; the playhead follows `audio.currentTime`.
- **Music clock:** one `<audio>` drives the slide scheduler from beat times + `syncOffsetMs` (saved). While casting, show the banner; the TV keeps fixed timing.
- **Second window:** main sends `sw://snapshot {v,state}` on open, then `sw://patch {v,ops}`. The window sends only `sw://intent {op}`. Heartbeat every 2 s; 6 s without one → "Not linked" and edits pause; on reconnect, request a snapshot. This fixes the black-window and wrong-order bugs.

### Proposed keys (check against your keymap)
| Key | Command |
|---|---|
| Ctrl+Shift+M | Make music video / show render |
| Ctrl+Shift+E | Export as music video |
| Ctrl+, · `?` · `/` | Settings · Cheat sheet · Tools |
| Ctrl+Shift+U | Open in window / Dock back |
| Ctrl+Shift+L · Shift+Space | Music tab · Music play/pause |
| Ctrl+←/→ | Previous/next song |
| Ctrl+Shift+C | Cast |
| `c` `k` `h` | Colour · Ken Burns · Hide controls |
| Ctrl+O · Ctrl+L · Ctrl+S | Open files · Playlist library · Save playlist as |
| Wizard only | Space, ←/→, Alt+←/→, L lock, W swap, R regenerate, G beat grid, T tap, Ctrl+B split, Ctrl+J merge, Ctrl+Z, Ctrl+Y, Ctrl+Enter render |

---

## 6. Copy deck (essentials)

Sentence case, no exclamation marks. `{braces}` are filled at runtime. Technical detail goes under **Details**, never deleted.

### Frame and steps
- Title: **Make music video** · `{project} · saved`
- Steps: Media · Song · Options · Analyze · Edit · Render · Done
- Footer: "Your files are only read, never changed."
- Close tooltip: "Close (Esc). Project is saved."

### Labels and progress
| Key | Text |
|---|---|
| amv.media.h | Pick photos and clips |
| amv.media.sub | SlideX picks the best moments. Untick anything you don't want used. |
| amv.song.h | Pick a song |
| amv.song.local | Local files only. SlideX never uploads anything or uses streaming services. |
| amv.opt.style.bal | Balanced: every 8 beats in calm parts, every 2 in the chorus, every beat on a drop. |
| amv.opt.len.hint | Takes the strongest {n} s (chorus and drop). Up to 10 minutes. |
| amv.opt.sound | Song only · video clips are muted |
| amv.opt.sound.keep | Keep clips' own sound quietly underneath · Coming later |
| amv.opt.out.hint | Never next to your originals. The project is kept so you can edit and render again. |
| amv.an.h | Analyzing · {i} of {n} · about {t} left |
| amv.an.media | Looking for dark, blurry, frozen or black stretches |
| amv.an.slow | Reading from {share} is slow (about {rate}). It will finish, just slower. Copying the files to this PC first is faster. |
| amv.an.fast | Everything was analyzed before. Ready in 1 s. |
| amv.ed.preview | Preview · low resolution · not rendered yet |
| amv.ed.lock | Lock / Locked — Kept when you regenerate (L) |
| amv.ed.trim | Start point inside the clip (length stays fixed) |
| amv.ed.undo | Undo · Redo — tooltip: Undo {step} (Ctrl+Z) / Nothing to undo |
| amv.ed.undo.toast | {step} done. · Undo → Undid: {step}. / Redid: {step}. |
| amv.ed.regen.toast | Re-planned {n} shots. {k} locked shots kept, {d} no longer fit and were unlocked. · Undo |
| grid.sub | Orange lines are where SlideX thinks the beats are. Line them up with the peaks while it plays. |
| rnd.h | {pct}% · Rendering shot {i} of {n} |
| rnd.bg | You can keep watching or editing your slideshow. Rendering continues in the background and when SlideX is minimized, but playback may stutter while it runs. Quitting SlideX asks first. |
| rnd.cancel | Stop rendering? Shots already done are kept, so the next render is faster. |
| done.h | Your music video is ready |
| legal.line | If you upload this video, a commercial song may get it muted or claimed. |
| mvx.sub | Renders your slideshow as it is, in playlist order, with its music. Nothing is picked or reordered. |
| music.castwarn | Casting to {tv}. Beat sync covers this PC's screens only. For a beat-synced show on the TV, export a music video and cast the file. |
| music.offset | Sync offset · {ms} ms · Saved · use if speakers or HDMI lag |
| cast.steps | Found {tv} · Opening a secure link to the TV… {s} s · Starting the stream |
| sw.busy | Browsing works. Changes you make are sent when the main window catches up, so they may take a few seconds to show. |
| sw.docked | Media Manager is back here. Order and selection kept. |

### Empty states
| Case | Title | Body | Actions |
|---|---|---|---|
| No media | No photos or clips yet | Pick what goes into the video. SlideX will choose the best moments. | Use current playlist · Open folder… |
| No song | Pick a song to set the rhythm | Cuts follow its beat. MP3, FLAC or WAV from this PC. | Choose song… |
| No music playlist | No music yet | Add songs to play under your slideshow. Slides can change on the beat. | Add music… · Add a folder… |
| Export, no music | This slideshow has no music. | Add songs in the Music tab first, then export. | Open Music tab |

### Errors (message · what to do · Details)
| Case | Message | What to do | Details |
|---|---|---|---|
| Tempo unclear | The beat in {song} isn't clear. | Fix the beat grid, or cut on a fixed timer instead. · Fix beat grid… · Cut every 2 s | confidence, candidate BPMs |
| Unsupported files | {n} files can't be used: {names} | The rest are fine. Skip these and carry on. · Skip and continue | reason per file |
| Missing file | {file} isn't where it was. | It may have been moved or renamed. · Locate… · Skip it | full path |
| Share unreachable | Can't reach {share}. {n} files live there. | Check the NAS is on and you're on the home network, then try again. · Try again · Locate folder… · Skip those {n} | Windows error code |
| Render failed | Rendering stopped at shot {i}. | Nothing was lost. Try again, or pick the Compatible encoder if it keeps failing. · Try again · Copy log path | ffmpeg exit code, stderr tail, log path |
| Disk space | Not enough space on {drive}. | This render needs about {need} (temporary files plus the video). {drive} has {free} free. · Save to another drive… · Try again | temp + output paths |
| Over 10 min | {song} is {dur}. Music videos can be up to 10:00. | Pick 30 s, 60 s or Custom to trim it. · Trim to 10:00 · Use 60 s | — |
| Too few items | Not enough material for {dur}. | {n} items cover about {cover} without repeats. Add more, shorten the video, or carry on and some shots will repeat. | — |
| Song unreadable | Couldn't read {file}. | The file may be damaged or not really an {ext}. Pick another song. | decoder error, log path |
| TV unreachable | Couldn't connect to {tv}. | Make sure the TV is on and on the same Wi-Fi. If Windows asked about the firewall, allow SlideX on Private networks. · Try again · Enter TV address | handshake error, cast.log path |
| Link dropped | Lost the link to the main window. | This is the last copy it sent. Editing is paused so nothing gets out of step. · Reconnect · Dock back | last snapshot version, age |

---

## 7. Optional ideas (not built)

1. History dropdown to jump back several undo steps at once.
2. Compare before/after Regenerate (hold a key to flip plans).
3. Pin a moment inside a video that must be used.
4. Remember beat-grid fixes per song across projects.
5. Render queue (several projects or shapes overnight).
6. Reveal a shot's source in the playlist.
7. Low-disk early warning on the Options step.
8. Quiet hours for toasts in Watch mode.
9. Printable shortcut cheat sheet for the second monitor.
10. Per-section style (Chill verse, Hype drop).

---

## 8. Open items to confirm

- [ ] Proposed keys vs your current keymap (`c`, `k`, `h`, `/`, `?`, the Ctrl+Shift set).
- [ ] *(proposed)* values: 10:00 cap, 1080p30, clips muted by default, in-house beat detection.
- [ ] Undo history cap (100) and whether history persists in the project file.
- [ ] Render-time and file-size estimates in the mockups are placeholders.
- [ ] Ken Burns and shortcut editor moving into Settings as sections (vs. staying separate modals linked from Settings).
