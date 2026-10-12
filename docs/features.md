# SlideX Features

SlideX is a single-file slideshow viewer and playlist tool. The current app includes:

## Media Import

- Open image and video files from the file picker.
- Drag and drop files onto the landing screen.
- Add more files later without restarting the session.
- Load playlists from JSON.

## Playback

- Play and pause slides.
- Move to the next or previous slide.
- Reverse playback.
- Adjust playback speed (toolbar buttons or keyboard: `↑` / `↓` and `,` / `.` as first-class co-defaults, ±5%; remappable via the key editor / `ssp_keymap` — do not use `[` / `]` which are sharpness). Toast shows the new rate when chrome is hidden.
- While a **video** is current, `←` / `→` (prev/next bindings) seek by the configured skip (**default ±5s**, range **1–30s**, persisted as `ssp_video_skip_sec`; toolbar **Skip** under More tools). **Double** the same key within **350ms** jumps to the previous/next playlist item immediately (a press after 350ms is a fresh seek). At the start, `←` goes to the previous media; at the end, `→` goes to the next. Images keep single-press slide prev/next.
- Video **seekbar**: click+drag the progress control to scrub the playhead (playing or paused). Seekbar scrub chrome is shown for **video** items only; stills keep a non-interactive slide-timer bar.
- Set a default duration for photos.
- Auto-advance through media during playback.

## Viewing Modes

- Fullscreen viewing.
- Watch mode with a cleaner presentation view.
- Hide and show controls while viewing.
- Toggle filename display.
- Show slide progress and current position.

## Image and Video Controls

- Zoom in and out.
- Reset pan and zoom.
- Videos show the **full frame** (contain, letterbox/pillarbox OK). Ken Burns motion stays **images-only**. Manual zoom and pan apply to the whole frame, or to the whole crop when one is set.
- Mirror horizontally or vertically.
- Rotate clockwise or counterclockwise.
- Mute playback and adjust volume.

## Color Controls

- **Themes** (`T`): cycle **10** named color presets (Sunset Glow, Cool Night, Mono Flash, Vivid Pop, Golden Hour, Soft Pastel, Film Noir, Teal Orange, Arctic Mist, Warm Vintage). Shift+`T` / Shift+Themes = random current; Alt+`T` / Alt+Themes = apply curated set across the playlist. Grades land in `imageUpdates.clr`.
- Adjust brightness.
- Adjust contrast.
- Adjust saturation.
- Adjust hue.
- Adjust highlights.
- Adjust lowlights (shadows).
- Adjust gamma (mids).
- Adjust sharpness (SVG convolve / baked convolution on export).
- Nudge lowlights / highlights / gamma / sharpness from the keyboard (defaults: Y/U, I/O, 5/6, [/]).
- Reset any color adjustment back to default.
- Grades persist per item in `imageUpdates.clr` (localStorage + playlist save).

## Layout Modes

- Horizontal Max mode for portrait media on landscape screens.
- Vertical Max mode for landscape media on portrait screens.
- 3-panel max modes for same-side layouts.

## Playlist

- Undock the Media Manager into its own native desktop window (Windows Tauri), including its toolbar (add/folder/shuffle/save/load). Dock back or close the window to restore chrome on the main window without losing playlist state.

## Playlist Management

- Open and manage playlists in a side panel.
- Shuffle the playlist.
- Create folders inside the playlist.
- Reorder items by drag and drop.
- Duplicate items in the playlist.
- Remove items from the playlist.
- Delete files from the system with playlist cleanup.
- Save and reload playlists.
- **Save playlist as…** stores a named playlist in the app data folder for `com.johnalindogan.slideshowpro` (one JSON file per name, written atomically). Saving over an existing name asks first. The file keeps the existing `{sspVersion, version, items, imageUpdates}` shape and adds `name`, `savedAt`, global `kb`, and `imageUpdates.crop`. A downloaded `SlideX_playlist.json` and an older `SlideShowX_playlist.json` still load.
- **Load playlist** lists name, item count, and last-saved date, with Rename and Delete (Delete asks first). **Open file…** still loads a downloaded JSON.
- Missing files stay in the list, labeled missing, and are skipped during playback. The toast reports how many are missing.
- **Locate folder…** rewrites a shared path prefix (drive letters and UNC, case-insensitive on Windows, on a path-segment boundary) only for files that exist at the new location. It shows a count such as `12 of 14 found` and saves only after confirm.

## Crop

- Crop is slideshow-only. The original file is never rewritten.
- Drag a rectangle with edge and corner handles. Aspect presets: Free, 16:9, 4:3, 1:1, 9:16, plus Reset. Same editor for photos and videos.
- The crop is stored as fractions of the displayed frame in a sidecar named `<file>.sspcrop.json` (`sspCropVersion`, app, source). If that folder is not writable, the sidecar is saved under app data and the screen says so. A missing or broken sidecar is ignored.
- The browser bitmap is already oriented, so a portrait phone photo crops the picture you see.

## Ken Burns

- Enable or disable Ken Burns motion.
- Control zoom intensity.
- Control pan intensity.
- Choose motion direction.
- Choose easing behavior.
- **5 named motion presets** (plus Auto / Off) in the Ken Burns modal and playlist per-slide override: Push In · Drift ↖, Pull Back · Drift ↘, Rise · Drift ↗, Descend · Drift ↙, Drift Across · Pan ↔. Images only — no Ken Burns on video.
- At the widest view the photo is fully visible (contain). Every Ken Burns move starts or ends on that whole image (the whole crop, when a crop is set) and only zooms in from there. Manual wheel, pinch, and drag use the same zoom and cannot zoom out past the whole image or drag it off screen. Manual interaction pauses Ken Burns on that slide; the next slide resumes it.

## Keyboard Shortcuts

- Customizable shortcut assignment editor (toolbar keyboard button) lists remappable commands from the app key map.
- Click a key badge and press a new key to assign; remaps persist in `localStorage` (`ssp_keymap`).
- Conflict detection: two commands cannot silently share a key (prior owner is unbound, with a toast).
- Per-command reset and **Reset all to defaults**.
- Defaults include navigation (`←`/`→`; context-sensitive seek on video), play/pause, speed ± (`↑`/`↓` and `,`/`.` co-defaults), zoom, fullscreen, mute, mirror, rotation, hide controls, filename toggle, edit nudges (`[`/`]` = sharpness), screenshot, and export graded (`E`).
- Fixed (not remappable): Delete / Backspace remove items, Alt+Backspace resets pan/zoom and does not remove the item, Alt+Arrows pan, Escape.
- View keys: `+` and `=` zoom in, `-` zooms out, `p` resets pan/zoom. All three stop at the whole image. Double-click on the stage resets too. `0` stays Hue +.

## Persistence

- Save per-item settings in local storage.
- Restore previous session data when available.
- Persist playlist and slide-specific adjustments.
- Store image adjustment metadata separately from file list entries.

## Export and Capture

- Save a screenshot of the current view with grades baked (including lowlights/highlights/gamma/sharpness), auto-saving to a Screenshots folder when supported.
- **Export Graded** (toolbar or `E`): for images, write a JPEG with the full color suite baked via the native save dialog (Tauri) or browser download. For video, best-effort canvas/MediaRecorder WebM bake for short clips (≤45s); otherwise export a `.sspgrade.json` sidecar documenting the grade. Graded export does not re-encode with ffmpeg. Cast TV copies use the ffmpeg build installed beside the app. Playlist save still keeps grades on the item. A crop is included in that same canvas path.
- **Export cropped**: writes a new file only (`name_crop.jpg`, or `name_crop (2).jpg` when that name is taken). JPEG quality 95, PNG when the source is PNG and the grade is still default, HEIC as JPEG. Size is the crop times the oriented resolution. Date-taken metadata is copied. A graded photo reuses the graded JPEG path. A video export is the current frame at the video's native resolution. The original is never overwritten.

## QA checklist

- Crop a portrait phone photo, a landscape photo, and a video. Restart and confirm the crop is still there, Ken Burns stays inside the crop, the original file hash is unchanged, and the console has no script errors.
- Export a cropped portrait photo, a graded landscape photo, and a video frame. Check each output's size, orientation, and colour. Originals stay untouched.
- Save two named playlists of about 20 mixed items. Restart and load both. Rename one file on disk and confirm that item shows as missing. Upgrade the app and confirm both playlists are still in app data.
- On ZB23, load a playlist that points at `\\AX03\G\...`, Locate folder against `\\AX03\Archive01`, restart, and confirm the rewritten paths persist. Items outside the old prefix stay put. `G:\Photos` does not match `G:\Photos2`.
- Ken Burns: portrait photo, landscape photo, and a very wide panorama. Zoom fully in and out, drag to each edge, and reset with `p`, double-click, and Alt+Backspace. `+`/`=` zoom in, `-` zooms out, and `0` still raises hue.

## Supported Media

- Images.
- MP4 video.
- MOV video.
- WebM video.

