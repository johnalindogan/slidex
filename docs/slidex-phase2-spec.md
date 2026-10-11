# SlideX Phase 2 spec

**Status:** draft. This change is documentation only.

## Where this file came from

`docs/slidex-phase2-spec.md` and `docs/live-mode-requirements.md` are absent from `main` (`31ea49a`, SlideX 0.1.8) and from every other branch on `origin`. The `docs/` tree on those branches holds `features.md`, `cast-spike.md`, and, on the Cast branch, `cast.md`. The open pull requests were checked as well: draft PR #35 (`cursor/slidex-cast-0-1-8-f9f9`, SlideX 0.1.9 Cast to a Google TV) and PR #7 (`fix/tauri-launch-path-ingest`). Neither contains a Phase 2 spec.

This file is new, branched from `main`, so the section below has a home. The auto-stitch milestone it extends is not written down in the repository. If that text shows up later, fold this section into it.

This section does not change **0.1.9** or **0.1.10**. Shipping version on `main` stays 0.1.8.

| Milestone | What this document does |
| --- | --- |
| Auto-stitch | Extended by [Auto-video from photos and videos](#auto-video-from-photos-and-videos). |
| 0.1.9 Cast to a Google TV | Unchanged. Still the open draft on PR #35. |
| 0.1.10 Ken Burns cast | Unchanged. Auto-video consumes the planner and the encoder that milestone builds. |

## Auto-video from photos and videos

Auto-video turns a set of photos and videos, plus the user's music, into one finished video. It extends the auto-stitch milestone: the same job of assembling a sequence, with a points-of-interest pass, beat-synced cuts, and subject-framed Ken Burns motion.

The run stays on the machine. Files are not uploaded.

### Triggers

Two entries start the same pipeline. Both run in the background, report progress, and stay fully local, with no upload.

| Trigger | Input | Output |
| --- | --- | --- |
| Watched folder | A folder named **Make a video**. Files that land there are the input. | A sibling output folder, beside the watched folder. |
| In-app **Make video** | A playlist or a selection, a music track, and a target length. | The finished video for that command. |

The watched folder needs no extra step once it is being watched: new media in that folder is the input, and the finished file is written next to it. The in-app command is the one that takes the music track and the target length with the playlist or the selection.

### Points-of-interest analysis

The analysis runs once per file. A repeat run with the same bytes and the same mtime reads the cache described below.

Videos are scored about once a second. Each sample records faces, saliency, motion, sharpness, exposure, and audio peaks. The pass keeps the best windows and skips stretches that are blurry, shaky, or dark.

Photos are scored once. Each photo gets face boxes and the most salient region.

| Signal | How it is measured |
| --- | --- |
| Faces | Face boxes from the face detector. |
| Saliency | The most salient region in the sample. |
| Motion | Motion in the sample. |
| Sharpness | Laplacian variance. Low variance marks a blurry stretch, which is skipped. |
| Exposure | Brightness of the sample. A dark stretch is skipped. |
| Audio peaks | Peaks in the clip's own audio. |
| Shake | Variance of optical-flow magnitude. A high variance marks a shaky stretch, which is skipped. |

Proposed models are small and permissively licensed. They run on the GPU through ONNX Runtime with the DirectML execution provider. Licenses below are the published claims. **Verify each license before adoption.**

| Job | Proposed model | Published license |
| --- | --- | --- |
| Face detection | YuNet | Apache-2.0 |
| Saliency | A small U2-Net-class model | Apache-2.0 |

Shake is measured from optical flow. It is not a separate learned model.

### Cache

The cache key is the content hash together with the file mtime. One entry per file. The entry is written after the first successful analysis and read on later runs.

**Hard requirement.** The analysis cache stores only boxes, scores and timestamps. It never stores face crops, face embeddings, or anything that identifies a person, and it lives only beside SlideX's own app data under `%LOCALAPPDATA%`.

SlideX's app data today is `%LOCALAPPDATA%\com.johnalindogan.slideshowpro\`. The cache sits beside that data, under `%LOCALAPPDATA%`, and has no other home. It is not written next to the source photos or videos.

The allowed payload is three kinds of value: boxes (a face box or a salient region, as coordinates in the frame), scores, and timestamps. The cache holds those values. It does not hold pixels of a face, a face embedding, a name, or any other identifier.

### Beat sync

Beat sync is required. Cuts, transitions, and pan/zoom changes align to the beats of the user's music.

The grid comes from the beat analyser. The M1 recommendation is [`web-audio-beat-detector`](https://github.com/chrisguttandin/web-audio-beat-detector) (MIT): a steady-tempo grid, with tap-to-adjust.

Window choice yields to that grid. For each beat-length slot, the assembler picks the analysis window that best fits the slot.

### Ken Burns

0.1.10's Ken Burns cast work builds the two pieces auto-video reuses:

- The shared pan-and-zoom planner, producing the same motion as on screen.
- The GPU encoder: WebCodecs H.264, muxed with [mp4-muxer](https://github.com/Vanilagy/mp4-muxer).

Auto-video feeds that planner the points of interest. Pans and zooms start and end on the subject, never the frame centre, and keep faces fully in frame.

0.1.10 itself is unchanged. This section only names the order in which the shared pieces are built and then consumed.

| Order | Work | Milestone |
| --- | --- | --- |
| 1 | Ken Burns cast: shared pan-and-zoom planner and GPU encoder | 0.1.10, unchanged |
| 2 | Points-of-interest analysis pass, including the cache | This section |
| 3 | Assembler, the watched **Make a video** folder, and in-app **Make video** | This section |

### Acceptance

1. 20 mixed files plus a song produce a finished video with no input.
2. Cuts land on beats within one frame.
3. Subjects are framed and faces are never cut off.
4. Bad footage is skipped.
5. A rerun uses the cache and is much faster (target at least 5×).
6. A log records each chosen window and why (scores).
7. cache contains no crops or embeddings.

Item 7 checks the cache hard requirement: a finished run's cache directory contains boxes, scores, and timestamps, and contains no face crops and no face embeddings.

### Early demo

Once the analysis pass exists, render a demo cut from a real folder John picks. Share that cut as an MP4 plus the selection log (each chosen window and its scores). Do this before the assembler UI is built.

### Open questions

| Question | Where it stands |
| --- | --- |
| Output resolution and frame rate | Proposal: 1080p30, with 4K optional. |
| Source-clip audio | Whether audio from the source clips ducks under the music. |
| Maximum video length | Unset. |
