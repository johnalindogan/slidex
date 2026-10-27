import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

const context = { console, Uint8Array, Date, Object, Math, String, Number, Array };
context.globalThis = context;
vm.runInNewContext(fs.readFileSync(new URL('./slidex-core.js', import.meta.url), 'utf8'), context);
const C = context.SlideXCore;

const KB = [
  { fs: 1.0, ts: 1.15, fx: 0, fy: 0, tx: -3, ty: -3 },
  { fs: 1.15, ts: 1.0, fx: -3, fy: -3, tx: 0, ty: 0 },
  { fs: 1.0, ts: 1.12, fx: 3, fy: 3, tx: -2, ty: -2 },
  { fs: 1.12, ts: 1.0, fx: -2, fy: 2, tx: 2, ty: -2 },
  { fs: 1.0, ts: 1.1, fx: -4, fy: 0, tx: 4, ty: 0 }
];
const ASPECTS = ['free', '16:9', '4:3', '1:1', '9:16'];

test('export size follows the displayed frame for EXIF orientations 1–8', () => {
  const rawW = 4000, rawH = 3001;
  const crop = { x: 0.1, y: 0.2, w: 0.3333, h: 0.4, aspect: 'free' };
  for (let o = 1; o <= 8; o++) {
    const d = C.orientedSize(rawW, rawH, o);
    if (o >= 5) { assert.equal(d.w, rawH); assert.equal(d.h, rawW); }
    else { assert.equal(d.w, rawW); assert.equal(d.h, rawH); }
    const size = C.exportPixelSize(rawW, rawH, o, crop);
    assert.ok(Math.abs(size.width - crop.w * d.w) < 1, `w ori ${o}`);
    assert.ok(Math.abs(size.height - crop.h * d.h) < 1, `h ori ${o}`);
    const full = C.exportPixelSize(rawW, rawH, o, { x: 0, y: 0, w: 1, h: 1 });
    assert.equal(full.width, d.w);
    assert.equal(full.height, d.h);
    const raw = C.displayedRectToRaw(0, 0, d.w, d.h, rawW, rawH, o);
    assert.ok(Math.abs(raw.w - rawW) < 1e-6 && Math.abs(raw.h - rawH) < 1e-6, `raw ori ${o}`);
  }
});

test('aspect presets lock the displayed crop for every EXIF orientation', () => {
  const frames = [];
  for (let o = 1; o <= 8; o++) frames.push([4000, 3000, o]);
  for (const [rawW, rawH, o] of frames) {
    const d = C.orientedSize(rawW, rawH, o);
    for (const aspect of ASPECTS) {
      const crop = C.applyAspect({ x: 0, y: 0, w: 1, h: 1 }, aspect, d.w, d.h);
      assert.ok(crop.x >= -1e-9 && crop.y >= -1e-9);
      assert.ok(crop.x + crop.w <= 1 + 1e-6 && crop.y + crop.h <= 1 + 1e-6);
      assert.equal(crop.aspect, aspect);
      if (aspect === 'free') continue;
      const px = C.displayedCropPixels(rawW, rawH, o, crop);
      const got = px.w / px.h;
      const want = C.ASPECTS[aspect];
      assert.ok(Math.abs(got - want) / want < 0.02, `${aspect} ori ${o} got ${got}`);
      const size = C.exportPixelSize(rawW, rawH, o, crop);
      assert.ok(Math.abs(size.width - px.w) < 1);
      assert.ok(Math.abs(size.height - px.h) < 1);
    }
  }
});

test('portrait orientation 6 maps a displayed top crop onto the stored pixels', () => {
  // 4000x3000 stored, orientation 6 displays as 3000x4000.
  const crop = { x: 0, y: 0, w: 1, h: 0.5, aspect: 'free' };
  const size = C.exportPixelSize(4000, 3000, 6, crop);
  assert.equal(size.width, 3000);
  assert.equal(size.height, 2000);
  const src = C.exportSourceRect(4000, 3000, 6, crop);
  assert.ok(Math.abs(src.sw - 2000) < 1);
  assert.ok(Math.abs(src.sh - 3000) < 1);
});

test('unique export names add (2) and never reuse the original', () => {
  assert.equal(C.cropDownloadName('Vacation.jpg', 'jpg'), 'Vacation_crop.jpg');
  assert.equal(C.cropDownloadName('shot.PNG', 'png'), 'shot_crop.png');
  assert.equal(C.uniqueExportName('Vacation_crop.jpg', []), 'Vacation_crop.jpg');
  assert.equal(C.uniqueExportName('Vacation_crop.jpg', ['Vacation_crop.jpg']), 'Vacation_crop (2).jpg');
  assert.equal(
    C.uniqueExportName('Vacation_crop.jpg', ['Vacation_crop.jpg', 'Vacation_crop (2).jpg']),
    'Vacation_crop (3).jpg'
  );
  assert.equal(C.uniqueExportName('Vacation_crop.jpg', ['vacation_crop.jpg']), 'Vacation_crop (2).jpg');
  assert.notEqual(C.cropDownloadName('Vacation.jpg', 'jpg'), 'Vacation.jpg');
  assert.equal(C.sidecarFileName('Vacation.jpg'), 'Vacation.jpg.sspcrop.json');
  assert.equal(C.samePath('G:\\Photos\\a.jpg', 'g:/photos/a.jpg'), true);
  assert.equal(C.samePath('G:\\Photos\\a.jpg', 'G:\\Photos\\a_crop.jpg'), false);
});

test('HEIC and JPEG export as JPEG, PNG stays PNG', () => {
  assert.equal(C.exportFormatForSource('a.heic', '').ext, 'jpg');
  assert.equal(C.exportFormatForSource('a.HEIC', 'image/heic').quality, 0.95);
  assert.equal(C.exportFormatForSource('a.jpg', 'image/jpeg').ext, 'jpg');
  assert.equal(C.exportFormatForSource('a.png', 'image/png').ext, 'png');
});

test('broken crop sidecars are ignored', () => {
  assert.equal(C.parseCropSidecar('not json'), null);
  assert.equal(C.parseCropSidecar('{"sspCropVersion":1}'), null);
  assert.equal(C.parseCropSidecar('{"sspCropVersion":2,"crop":{"x":0,"y":0,"w":1,"h":1}}'), null);
  const ok = C.parseCropSidecar('{"sspCropVersion":1,"crop":{"x":0.1,"y":0.2,"w":0.5,"h":0.4,"aspect":"16:9"}}');
  assert.equal(ok.aspect, '16:9');
  assert.ok(Math.abs(ok.w - 0.5) < 1e-9);
});

test('playlist round trip keeps order, crop, grade, timing, and Ken Burns', () => {
  const doc = C.buildPlaylistDocument({
    name: 'Weekend',
    savedAt: 1700000000000,
    kb: { enabled: true, zoomInt: 4, panInt: 4, direction: 'mix', easing: 'ease' },
    items: [
      { type: 'folder', id: 'f1', name: 'Day', open: true },
      {
        id: 'a', type: 'image', name: 'portrait.jpg', path: 'G:\\Photos\\portrait.jpg',
        clr: { b: 110, c: 100, s: 120, h: 5, hl: 0, sh: 0, g: 100, shp: 110 },
        customDur: 8000, kbPreset: 2, zoom: 1.05, panX: 0, panY: 0, rotation: 0,
        crop: { x: 0.1, y: 0.05, w: 0.8, h: 0.9, aspect: '4:3' }
      },
      {
        id: 'b', type: 'video', name: 'clip.mp4', path: 'G:\\Photos\\clip.mp4',
        clr: C.CLR_DEF, customDur: undefined, kbPreset: -1, crop: null
      }
    ]
  });
  assert.equal(doc.sspVersion, 1);
  const back = C.normalizePlaylist(JSON.parse(JSON.stringify(doc)));
  assert.equal(back.name, 'Weekend');
  assert.equal(back.items[0].type, 'folder');
  assert.deepEqual(back.items.map(it => it.path || it.name), ['Day', 'G:\\Photos\\portrait.jpg', 'G:\\Photos\\clip.mp4']);
  const a = C.restoreItemFields(back.items[1]);
  assert.equal(a.customDur, 8000);
  assert.equal(a.kbPreset, 2);
  assert.equal(a.clr.b, 110);
  assert.equal(a.clr.shp, 110);
  assert.ok(Math.abs(a.crop.w - 0.8) < 1e-9);
  assert.equal(a.crop.aspect, '4:3');
  const b = C.restoreItemFields(back.items[2]);
  assert.equal(b.kbPreset, -1);
  assert.equal(b.crop, null);
});

test('legacy SlideX and SlideShowX playlist JSON loads', () => {
  const slideshowx = {
    version: 2,
    items: [{
      id: 'old', type: 'image', name: 'lake.jpg', path: 'D:\\lake.jpg',
      imageUpdates: { clr: { b: 90 }, customDur: 3000, kbPreset: 0 }
    }]
  };
  const legacy = C.normalizePlaylist(slideshowx);
  assert.equal(legacy.sspVersion, 1);
  const item = C.restoreItemFields(legacy.items[0]);
  assert.equal(item.path, 'D:\\lake.jpg');
  assert.equal(item.customDur, 3000);
  assert.equal(item.kbPreset, 0);
  assert.equal(item.clr.b, 90);

  const slidex = {
    sspVersion: 1, version: 2, name: 'Trip',
    items: [{ type: 'image', name: 'a.jpg', path: 'G:\\a.jpg', imageUpdates: { crop: { x: 0, y: 0, w: 0.5, h: 1, aspect: 'free' } } }]
  };
  const cur = C.normalizePlaylist(slidex);
  assert.equal(cur.sspVersion, 1);
  assert.equal(C.restoreItemFields(cur.items[0]).crop.w, 0.5);
  assert.throws(() => C.normalizePlaylist({ name: 'nope' }));
});

test('session restore counts one file missing from disk and ignores entries with no path', () => {
  const items = [
    { type: 'folder', name: 'F' },
    { id: 'a', type: 'image', name: 'here.jpg', path: 'D:\\here.jpg' },
    { id: 'b', type: 'image', name: 'gone.jpg', path: '\\\\AX03\\share\\gone.jpg' },
    { id: 'c', type: 'image', name: 'nopath.jpg' }
  ];
  const plan = C.planSessionRestore(items, (p) => p === 'D:\\here.jpg');
  assert.equal(plan.missing, 1);
  assert.equal(C.missingFilesNotice(plan.missing), "1 file couldn't be found");
  assert.equal(plan.items.length, 4);
  assert.equal(plan.items[1].missing, false);
  assert.equal(plan.items[2].missing, true);
  assert.equal(plan.items[2].unavailable, true);
  assert.equal(plan.items[2].path, '\\\\AX03\\share\\gone.jpg');
  assert.ok(!plan.items[3].missing);
  assert.equal(items[2].missing, undefined);
});

test('session restore counts every file missing from disk', () => {
  const items = [
    { id: 'a', type: 'image', name: 'a.jpg', path: '\\\\AX03\\share\\a.jpg' },
    { id: 'b', type: 'video', name: 'b.mp4', path: '\\\\AX03\\share\\b.mp4' }
  ];
  const plan = C.planSessionRestore(items, () => false);
  assert.equal(plan.missing, 2);
  assert.equal(C.missingFilesNotice(plan.missing), "2 files couldn't be found");
  assert.equal(plan.items.length, 2);
  assert.equal(plan.items[0].missing, true);
  assert.equal(plan.items[1].missing, true);
});

test('a 0.1.8 session with \\\\?\\ paths is fully present and the saved strings stay', () => {
  const session = {
    sspVersion: 1,
    savedAt: 1710000000000,
    items: [
      {
        id: 'a',
        type: 'image',
        name: 'lake.jpg',
        path: '\\\\?\\C:\\Users\\qa\\Pictures\\lake.jpg',
        imageUpdates: { customDur: 10000 }
      },
      {
        id: 'b',
        type: 'image',
        name: 'dock.jpg',
        path: '\\\\?\\UNC\\AX03\\photos\\dock.jpg',
        imageUpdates: { customDur: 8000 }
      }
    ]
  };
  const plan = C.planSessionRestore(session.items, () => true);
  assert.equal(plan.missing, 0);
  assert.equal(C.missingFilesNotice(plan.missing), '');
  assert.equal(plan.items[0].missing, false);
  assert.equal(plan.items[0].unavailable, undefined);
  assert.equal(plan.items[0].path, '\\\\?\\C:\\Users\\qa\\Pictures\\lake.jpg');
  assert.equal(plan.items[1].path, '\\\\?\\UNC\\AX03\\photos\\dock.jpg');
  assert.equal(plan.items[1].imageUpdates.customDur, 8000);
  const doc = C.buildPlaylistDocument({ items: plan.items, savedAt: session.savedAt });
  assert.equal(doc.items.length, 2);
  assert.equal(doc.items[0].path, session.items[0].path);
  assert.equal(doc.items[1].path, session.items[1].path);
});

test('session restore keeps missing entries so the next save still has their paths', () => {
  const items = [
    { id: 'keep', type: 'image', name: 'keep.jpg', path: 'D:\\keep.jpg', imageUpdates: { customDur: 4000 } },
    { id: 'gone', type: 'image', name: 'gone.jpg', path: '\\\\AX03\\share\\gone.jpg', imageUpdates: { customDur: 8000 } }
  ];
  const plan = C.planSessionRestore(items, (p) => p === 'D:\\keep.jpg');
  const doc = C.buildPlaylistDocument({ items: plan.items, savedAt: 1 });
  assert.equal(doc.items.length, 2);
  assert.equal(doc.items[0].path, 'D:\\keep.jpg');
  assert.equal(doc.items[1].path, '\\\\AX03\\share\\gone.jpg');
  assert.equal(plan.items[1].imageUpdates.customDur, 8000);
  assert.equal(items[1].path, '\\\\AX03\\share\\gone.jpg');
});

test('missing items are marked and skipped without dropping the playlist', () => {
  const items = [
    { type: 'folder', name: 'F' },
    { type: 'image', name: 'keep.jpg', path: 'G:\\keep.jpg' },
    { type: 'image', name: 'gone.jpg', path: 'G:\\gone.jpg' },
    { type: 'video', name: 'gone.mp4', path: 'G:\\gone.mp4' }
  ];
  const marked = C.markMissing(items, (p) => p === 'G:\\keep.jpg');
  assert.equal(marked.missing, 2);
  assert.equal(marked.items.length, 4);
  assert.equal(marked.items[0].type, 'folder');
  assert.equal(marked.items[1].missing, false);
  assert.equal(marked.items[2].missing, true);
  assert.equal(marked.items[2].unavailable, true);
  assert.deepEqual(C.slideshowItems(marked.items).map(it => it.name), ['keep.jpg']);
});

test('prefix rewrite: drive to UNC, UNC to UNC, case, and segment boundaries', () => {
  const ci = { caseInsensitive: true };
  assert.equal(
    C.rewritePathPrefix('G:\\Photos\\Vacation\\a.jpg', 'G:\\', '\\\\AX03\\Archive01', ci),
    '\\\\AX03\\Archive01\\Photos\\Vacation\\a.jpg'
  );
  assert.equal(
    C.rewritePathPrefix('\\\\AX03\\G\\Photos\\a.jpg', '\\\\AX03\\G', '\\\\AX03\\Archive01', ci),
    '\\\\AX03\\Archive01\\Photos\\a.jpg'
  );
  assert.equal(
    C.rewritePathPrefix('\\\\old\\share\\b.jpg', '\\\\old\\share', '\\\\AX03\\Archive01', ci),
    '\\\\AX03\\Archive01\\b.jpg'
  );
  assert.equal(
    C.rewritePathPrefix('g:\\photos\\A.JPG', 'G:\\Photos', '\\\\AX03\\Archive01', ci),
    '\\\\AX03\\Archive01\\A.JPG'
  );
  assert.equal(C.rewritePathPrefix('G:\\Photos2\\a.jpg', 'G:\\Photos', '\\\\AX03\\Archive01', ci), null);
  assert.equal(C.rewritePathPrefix('G:\\Photos\\a.jpg', 'G:\\Photo', 'D:\\X', ci), null);
  assert.equal(C.rewritePathPrefix('D:\\Other\\b.jpg', 'G:\\', '\\\\AX03\\Archive01', ci), null);
});

test('playlist name keys ignore case while the saved display name stays as typed', () => {
  assert.equal(C.playlistNameKey('Beach'), C.playlistNameKey('beach'));
  assert.equal(C.playlistNameKey(' Beach '), C.playlistNameKey('BEACH'));
  assert.equal(C.playlistNamesMatch('Beach', 'beach'), true);
  assert.equal(C.playlistNamesMatch('Beach', 'Shore'), false);
  const doc = C.buildPlaylistDocument({ name: 'Beach', items: [{ id: '1', type: 'image', name: 'a.jpg', path: 'G:\\a.jpg' }] });
  assert.equal(doc.name, 'Beach');
});

test('locate probing stops at the first matching parent', () => {
  const n = 40;
  const files = [];
  for (let i = 0; i < n; i++) files.push('\\\\AX03\\Archive01\\Show\\Reel\\Take\\Clip' + i + '.mp4');
  const ancestors = C.ancestorPrefixes(files[0]).length;
  assert.ok(ancestors > 1);
  let probes = 0;
  const exists = (p) => {
    probes++;
    return /\\zb23\\media\\clip\d+\.mp4$/i.test(String(p));
  };
  const plan = C.planLocate(files, '\\\\ZB23\\Media', exists, { caseInsensitive: true });
  assert.equal(plan.found, n);
  assert.equal(probes, n);
  assert.ok(n * ancestors > probes);
});

test('locate plan only rewrites paths that exist and leaves outsiders', () => {
  const missing = [
    'G:\\Photos\\a.jpg',
    'G:\\Photos\\b.jpg',
    'D:\\Other\\c.jpg',
    'G:\\Photos2\\d.jpg'
  ];
  const have = new Set([
    '\\\\ax03\\archive01\\photos\\a.jpg',
    '\\\\ax03\\archive01\\photos\\b.jpg'
  ]);
  const exists = (p) => have.has(String(p).toLowerCase());
  const plan = C.planLocate(missing, '\\\\AX03\\Archive01', exists, { caseInsensitive: true });
  assert.equal(plan.found, 2);
  assert.ok(plan.eligible >= 2);
  const byPath = Object.fromEntries(plan.rows.map(r => [r.path, r]));
  assert.equal(byPath['G:\\Photos\\a.jpg'].exists, true);
  assert.equal(byPath['G:\\Photos\\b.jpg'].exists, true);
  assert.equal(byPath['G:\\Photos\\a.jpg'].rewritten, '\\\\AX03\\Archive01\\Photos\\a.jpg');
  assert.equal(byPath['D:\\Other\\c.jpg'].outside, true);
  assert.equal(byPath['G:\\Photos2\\d.jpg'].exists, false);
  assert.equal(
    C.rewritePathPrefix('G:\\Photos2\\d.jpg', 'G:\\Photos', '\\\\AX03\\Archive01', { caseInsensitive: true }),
    null
  );
});

test('contain and clamp: portrait, landscape, panorama, and a crop', () => {
  const vp = { w: 1920, h: 1080 };
  const portrait = C.containRect(vp.w, vp.h, 1080, 1920);
  assert.ok(Math.abs(portrait.h - 1080) < 1e-6);
  assert.ok(portrait.w < vp.w - 10);
  assert.ok(portrait.x > 0);

  const landscape = C.containRect(vp.w, vp.h, 4000, 3000);
  assert.ok(Math.abs(landscape.w - 1440) < 1e-6 || Math.abs(landscape.h - 1080) < 1e-6);
  assert.ok(landscape.w <= vp.w + 1e-6 && landscape.h <= vp.h + 1e-6);

  const pano = C.containRect(vp.w, vp.h, 6000, 800);
  assert.ok(Math.abs(pano.w - 1920) < 1e-6);
  assert.ok(pano.h < vp.h - 10);

  const square = C.containRect(vp.w, vp.h, 1000, 1000);
  assert.ok(Math.abs(square.w - square.h) < 1e-6);
  assert.ok(square.h <= vp.h + 1e-6);

  const floored = C.clampView({ zoom: 0.2, panX: 400, panY: 400 }, vp.w, vp.h, 1080, 1920);
  assert.equal(floored.zoom, 1);
  assert.ok(Math.abs(floored.panX) < 1e-6 && Math.abs(floored.panY) < 1e-6);

  const dragged = C.clampView({ zoom: 3, panX: 99999, panY: -99999 }, vp.w, vp.h, 6000, 800);
  const rect = C.placedRect(dragged, vp.w, vp.h, 6000, 800);
  assert.ok(rect.x <= 0.01);
  assert.ok(rect.x + rect.w >= vp.w - 0.01);
  assert.equal(dragged.zoom, 3);

  const portraitZoom = C.clampView({ zoom: 2, panX: 5000, panY: 5000 }, vp.w, vp.h, 1080, 1920);
  const prect = C.placedRect(portraitZoom, vp.w, vp.h, 1080, 1920);
  assert.ok(prect.y <= 0.01 && prect.y + prect.h >= vp.h - 0.01);
  // Still pillarboxed horizontally, so it stays centered on X.
  assert.ok(Math.abs(prect.x - (vp.w - prect.w) / 2) < 1e-4);
});

test('zoom toward the pointer does not go below the whole image', () => {
  const before = { zoom: 1, panX: 0, panY: 0 };
  const zoomed = C.zoomToward(before, 2, 100, 540, 1920, 1080, 1080, 1920);
  assert.ok(zoomed.zoom >= 1.9 && zoomed.zoom <= 2);
  const back = C.zoomToward(zoomed, 0.01, 100, 540, 1920, 1080, 1080, 1920);
  assert.equal(back.zoom, 1);
  assert.ok(Math.abs(back.panX) < 1e-6 && Math.abs(back.panY) < 1e-6);
});

test('Ken Burns wide end is the whole image and the other end only zooms in', () => {
  for (const preset of KB) {
    const ends = C.wholeImageEndpoints(preset, 4, 4);
    const wide = ends.from.s <= ends.to.s ? ends.from : ends.to;
    const close = ends.from.s <= ends.to.s ? ends.to : ends.from;
    assert.equal(wide.s, 1);
    assert.equal(wide.x, 0);
    assert.equal(wide.y, 0);
    assert.ok(close.s >= 1);
  }
});

test('Alt+Backspace resets and does not change the playlist count; 0 is not reset', () => {
  const alt = { key: 'Backspace', altKey: true, ctrlKey: false, metaKey: false, shiftKey: false };
  const plain = { key: 'Backspace', altKey: false, ctrlKey: false, metaKey: false, shiftKey: false };
  assert.equal(C.isAltBackspaceReset(alt), true);
  assert.equal(C.willRemovePlaylistItem(alt), false);
  assert.equal(C.playlistCountAfterKey(5, alt), 5);
  assert.equal(C.willRemovePlaylistItem(plain), true);
  assert.equal(C.playlistCountAfterKey(5, plain), 4);
  assert.equal(C.isHueUpKey('0'), true);
  assert.equal(C.isResetPanKey('0'), false);
  assert.equal(C.isZoomInKey('+'), true);
  assert.equal(C.isZoomInKey('='), true);
  assert.equal(C.isZoomOutKey('-'), true);
  assert.equal(C.isResetPanKey('p'), true);
});

test('date-taken survives a JPEG and PNG rewrite and a loose HEIC scan', () => {
  const date = '2019:07:04 12:30:45';
  const jpeg = C.injectJpegExif(new Uint8Array([0xff, 0xd8, 0xff, 0xd9]), date);
  assert.equal(C.readJpegDate(jpeg), date);
  const sig = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]);
  const iend = new Uint8Array([0, 0, 0, 0, 73, 69, 78, 68, 0xae, 0x42, 0x60, 0x82]);
  const pngIn = new Uint8Array(sig.length + iend.length);
  pngIn.set(sig, 0); pngIn.set(iend, sig.length);
  const png = C.injectPngDate(pngIn, date);
  assert.equal(C.readPngDate(png), date);
  const heic = new Uint8Array(32);
  const stamp = '2020:01:02 03:04:05';
  for (let i = 0; i < stamp.length; i++) heic[4 + i] = stamp.charCodeAt(i);
  const copied = C.copyDateTaken(heic, new Uint8Array([0xff, 0xd8, 0xff, 0xd9]));
  assert.equal(C.readJpegDate(copied), stamp);
});

test('cast hook crops stills on the fly and refuses cropped video on TV', () => {
  const still = C.castMediaHook({ type: 'image', path: 'G:\\a.jpg', crop: { x: 0, y: 0, w: 0.5, h: 0.5, aspect: '1:1' } });
  assert.equal(still.kind, 'still');
  assert.equal(still.cropOnTv, true);
  assert.ok(still.crop && still.crop.w === 0.5);
  const video = C.castMediaHook({ type: 'video', path: 'G:\\a.mp4', crop: { x: 0, y: 0, w: 0.5, h: 1, aspect: 'free' } });
  assert.equal(video.cropOnTv, false);
  assert.match(video.message, /can't be cropped on the TV/);
});

test('the slideshow wires Alt+Backspace before Backspace removal and keeps Hue + on 0', () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const resetAt = html.indexOf('isAltBackspaceReset');
  const backAt = html.indexOf("if (e.key === 'Backspace')");
  assert.ok(resetAt !== -1 && backAt !== -1 && resetAt < backAt);
  assert.match(html, /id:'hueUp',\s+label:'Hue \+',\s+def:'0'/);
  assert.match(html, /k === '=' && keysEqual\(keyMap\.zoomIn, '\+'\)/);
  assert.match(html, /id:'zoomIn',\s+label:'Zoom In',\s+def:'\+'/);
  assert.match(html, /id:'zoomOut',\s+label:'Zoom Out',\s+def:'-'/);
  assert.match(html, /id:'panRst',\s+label:'Reset Pan\/Zoom',\s+def:'p'/);
});

test('background load status names the current file and never blocks clicks', () => {
  const mid = C.backgroundLoadStep(3, 12);
  assert.equal(mid.text, 'Loading 3 of 12…');
  assert.equal(mid.visible, true);
  assert.equal(mid.blocksClicks, false);
  assert.equal(C.loadingStatusText(3, 12), 'Loading 3 of 12…');
  const done = C.backgroundLoadStep(13, 12);
  assert.equal(done.visible, false);
  assert.equal(done.text, '');
  assert.equal(done.blocksClicks, false);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /id="loadstatus"/);
  assert.match(html, /#loadstatus\{[^}]*pointer-events:none/);
  assert.doesNotMatch(html, /id="loadstatus"[^>]*aria-modal/);
  assert.match(html, /showLoadStatus\(i \+ 1, total\)/);
  assert.match(html, /await yieldToMain\(\)/);
});

test('default image duration is 10 seconds and a saved per-item duration wins', () => {
  assert.equal(C.DEFAULT_IMAGE_DURATION_MS, 10000);
  assert.ok(C.IMAGE_DURATION_CHOICES_MS.includes(10000));
  assert.equal(C.savedImageDuration(null), 10000);
  assert.equal(C.savedImageDuration('8000'), 8000);
  assert.equal(C.savedImageDuration('999'), 3000);
  assert.equal(C.resolveImageDuration(undefined, 10000), 10000);
  assert.equal(C.resolveImageDuration(4000, 10000), 4000);
  assert.equal(C.resolveImageDuration(20000, 10000), 20000);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /value="10000" selected>10s/);
  assert.match(html, /ssp_default_dur/);
  assert.match(html, /\[10000,'10s'\]/);
});

test('a saved duration off the list falls back to the closest choice', () => {
  const lines = [];
  const orig = console.log;
  console.log = function () {
    lines.push(Array.prototype.join.call(arguments, ' '));
  };
  context.window = {
    __TAURI__: {
      core: {
        invoke: function (cmd, args) {
          lines.push(cmd + ' from=' + args.fromMs + ' to=' + args.toMs);
          return Promise.resolve();
        }
      }
    }
  };
  try {
    lines.length = 0;
    assert.equal(C.savedImageDuration('999'), 3000);
    assert.deepEqual(lines, ['cast_log_dur_fallback from=999 to=3000']);
    lines.length = 0;
    assert.equal(C.savedImageDuration('3500'), 3000);
    assert.deepEqual(lines, ['cast_log_dur_fallback from=3500 to=3000']);
    lines.length = 0;
    assert.equal(C.savedImageDuration('25000'), 20000);
    assert.deepEqual(lines, ['cast_log_dur_fallback from=25000 to=20000']);
    lines.length = 0;
    assert.equal(C.savedImageDuration('20000'), 20000);
    assert.equal(C.savedImageDuration(null), 10000);
    assert.equal(C.savedImageDuration(''), 10000);
    assert.equal(C.savedImageDuration('nope'), 10000);
    assert.deepEqual(lines, []);
    assert.equal(C.castPhotoHoldMs(null, 999), 3000);
  } finally {
    console.log = orig;
    delete context.window;
  }
});

test('dur_fallback invoke uses camelCase argument names', async () => {
  const calls = [];
  context.window = {
    __TAURI__: {
      core: {
        invoke: function (cmd, args) {
          calls.push({ cmd: cmd, args: args });
          return Promise.reject(new Error('rejected'));
        }
      }
    }
  };
  try {
    assert.equal(C.savedImageDuration('999'), 3000);
    await Promise.resolve();
    assert.equal(calls.length, 1);
    assert.equal(calls[0].cmd, 'cast_log_dur_fallback');
    assert.deepEqual(Object.keys(calls[0].args).sort(), ['fromMs', 'toMs']);
    assert.equal(calls[0].args.fromMs, 999);
    assert.equal(calls[0].args.toMs, 3000);
    assert.equal(calls[0].args.from_ms, undefined);
    assert.equal(calls[0].args.to_ms, undefined);
  } finally {
    delete context.window;
  }
});

test('the cast timer skips unsupported files without another photo wait', () => {
  const items = [
    { name: 'one.jpg', path: '/p/one.jpg' },
    { name: 'bad.mp4', path: '/p/bad.mp4' },
    { name: 'skip.webp', path: '/p/skip.webp' },
    { name: 'three.jpg', path: '/p/three.jpg' },
  ];
  const opts = {
    skippedNotes: ['skip.webp: Unsupported media. Cast plays JPEG, PNG, and MP4 (H.264 + AAC). Skipped.'],
    badPaths: { '/p/bad.mp4': true },
  };
  assert.equal(C.nextIndexSkippingUnsupported(0, items, opts), 3);
  assert.equal(C.nextIndexSkippingUnsupported(3, items, opts), 0);
  assert.equal(C.nextIndexSkippingUnsupported(0, items, { reverse: true, badPaths: { '/p/three.jpg': true } }), 2);
  assert.equal(C.nextIndexSkippingUnsupported(0, [{ name: 'only.jpg', path: '/p/only.jpg' }], { badPaths: {} }), null);
  assert.equal(C.nextIndexSkippingUnsupported(0, items, {
    badPaths: { '/p/bad.mp4': true, '/p/skip.webp': true, '/p/three.jpg': true },
  }), null);

  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const sched = html.slice(html.indexOf('function schedNext()'), html.indexOf('function goTo('));
  assert.match(sched, /goTo\(nextMI\(\), 'timer'\)/);
  assert.match(sched, /if \(!playing \|\| isV\(cur\)\) return/);
  assert.match(sched, /typeof window\.SlideXCore\.nextIndexSkippingUnsupported === "function"/);

  const timers = [];
  let seq = 1;
  const visits = [];
  const sandbox = {
    console,
    SlideXCore: C,
    cur: 0,
    playing: true,
    tmr: null,
    reversePlay: false,
    items,
    getMediaItems() { return sandbox.items; },
    isV() { return false; },
    nextMI() { return (sandbox.cur + 1) % sandbox.items.length; },
    getEDur() { return 10000; },
    setTimeout(fn, ms) { const id = seq++; timers.push({ id, fn, ms }); return id; },
    clearTimeout() {},
    goTo(i, source) { visits.push({ i, source }); sandbox.cur = i; },
    window: {
      SlideXCore: C,
      __sspCastActive: true,
      __sspCastSkipped: opts.skippedNotes,
      __sspCastBadPaths: opts.badPaths,
    },
  };
  vm.runInNewContext(sched, sandbox);
  sandbox.schedNext();
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 10000);
  timers[0].fn();
  assert.deepEqual(visits, [{ i: 3, source: 'timer' }]);
  assert.equal(timers.length, 2, 'the landing photo arms one timer, not one per skipped file');
});

test('a 20s cast photo uses that duration, and a change waits for the next photo', () => {
  assert.equal(C.savedImageDuration('20000'), 20000);
  assert.equal(C.castPhotoHoldMs(null, 20000), 20000);
  assert.equal(C.castPhotoHoldMs({ customDur: 20000 }, 10000), 20000);
  assert.equal(C.castPhotoHoldMs({ imageUpdates: { customDur: 20000 } }, 10000), 20000);
  assert.equal(C.castPhotoHoldMs(null, 10000), 10000);
  assert.equal(C.castPhotoHoldMs(null, 999), 3000);
  assert.equal(C.resolveImageDuration(undefined, 20000), 20000);

  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /value="20000">20s/);
  assert.doesNotMatch(html, /__sspCastHoldMs/);
  const durFns = html.slice(html.indexOf('const getDur = () =>'), html.indexOf('/* ── Zoom / Pan ── */'));
  const sched = html.slice(html.indexOf('function schedNext()'), html.indexOf('function goTo('));
  const onchange = html.slice(html.indexOf("g('dsel').onchange"), html.indexOf('(function initDefaultImageDuration'));
  const hook = html.slice(html.indexOf('function sspCastHook('), html.indexOf('function sspCastPlaying'));
  assert.match(onchange, /__sspCastActive/);
  assert.ok(onchange.indexOf('return') < onchange.indexOf('clearTimeout(tmr)'));
  const castBranch = onchange.slice(onchange.indexOf('__sspCastActive'), onchange.indexOf('clearTimeout(tmr)'));
  assert.match(castBranch, /startKB\(cur\)/);
  assert.match(castBranch, /startProg\(\)/);
  assert.doesNotMatch(castBranch, /schedNext/);
  assert.match(sched, /goTo\(nextMI\(\), 'timer'\)/);
  assert.doesNotMatch(sched, /__sspCastHoldMs/);
  assert.match(hook, /getEDur\(\)/);
  const load = html.slice(html.indexOf('function loadCurrent'), html.indexOf('function syncTransport'));
  assert.match(load, /src === ['"]timer['"]/);
  assert.match(load, /durationMs/);
  assert.doesNotMatch(load, /__sspCastHoldMs/);
  const slideStart = html.indexOf('slide: function (source, holdMs)');
  const slideFn = html.slice(slideStart, html.indexOf('transport: function', slideStart)).replace(/,\s*$/, '');
  assert.doesNotMatch(slideFn, /getEDur|__sspCastHoldMs/);
  assert.ok(slideFn.indexOf('holdMs') < slideFn.indexOf('enqueue'), 'the loaded duration is captured before the load is queued');

  const timers = [];
  const loads = [];
  let seq = 1;
  let kb = 0;
  let prog = 0;
  const sandbox = {
    console,
    SlideXCore: C,
    cur: 0,
    playing: true,
    tmr: null,
    ptmr: null,
    spd: 100,
    dsel: '20000',
    dselEl: { value: '20000' },
    items: [
      { name: 'one.jpg' },
      { name: 'two.jpg', customDur: 12000 },
      { name: 'three.jpg' },
    ],
    castActive: true,
    getMediaItems() { return sandbox.items; },
    getSpd() { return sandbox.spd / 100; },
    isV() { return false; },
    nextMI() { return (sandbox.cur + 1) % sandbox.items.length; },
    g(id) {
      if (id === 'dsel') {
        sandbox.dselEl.value = sandbox.dsel;
        return sandbox.dselEl;
      }
      return {};
    },
    localStorage: { setItem(k, v) { this[k] = v; } },
    startKB() { kb += 1; },
    startProg() { prog += 1; },
    clearInterval() {},
    enqueue(fn) { fn(); },
    loadCurrent(source, holdMs) { loads.push({ source, holdMs }); },
    setTimeout(fn, ms) {
      const id = seq++;
      timers.push({ id, fn, ms });
      return id;
    },
    clearTimeout(id) {
      const i = timers.findIndex(t => t.id === id);
      if (i >= 0) timers.splice(i, 1);
    },
    goTo(i, source) {
      sandbox.cur = i;
      sandbox.sspCastHook('slide', source);
    },
    window: { SlideXCore: C, __sspCastActive: true, __sspCastGen: 1 },
  };
  sandbox.window.window = sandbox.window;
  vm.runInNewContext(`
    ${durFns}
    ${sched}
    ${hook}
    ${onchange}
    window.__sspCast = { ${slideFn} };
  `, sandbox);
  sandbox.schedNext();
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 20000);
  sandbox.dsel = '8000';
  sandbox.dselEl.onchange();
  assert.equal(kb, 1, 'a duration change while casting restarts Ken Burns');
  assert.equal(prog, 1, 'a duration change while casting restarts the progress bar');
  assert.equal(timers.length, 1, 'changing duration while casting must not restart this photo');
  assert.equal(timers[0].ms, 20000);
  sandbox.playing = false;
  kb = 0;
  prog = 0;
  sandbox.dselEl.onchange();
  assert.equal(kb, 1, 'a paused cast still restarts Ken Burns');
  assert.equal(prog, 1, 'a paused cast still restarts the progress bar');
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 20000, 'a paused cast does not restart the photo timer');
  sandbox.playing = true;
  const fired = timers[0];
  timers.splice(0, 1);
  fired.fn();
  assert.equal(sandbox.cur, 1);
  assert.deepEqual(loads, [{ source: 'timer', holdMs: 12000 }], 'dur_ms is the loaded item, not the 20s hold that just ended');
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 12000, 'the loaded photo keeps its own duration');
  const firedNext = timers[0];
  timers.splice(0, 1);
  firedNext.fn();
  assert.equal(sandbox.cur, 2);
  assert.deepEqual(loads[1], { source: 'timer', holdMs: 8000 });
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 8000, 'a photo without its own duration uses the setting changed during the previous one');
  sandbox.window.__sspCastActive = false;
  sandbox.cur = 0;
  sandbox.dsel = '6000';
  kb = 0;
  prog = 0;
  sandbox.dselEl.onchange();
  assert.equal(kb, 1);
  assert.equal(prog, 1);
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 6000, 'off cast, the photo timer restarts at the new duration');
});

test('0.1.18 versions match and the welcome screen reads getVersion()', () => {
  const pkg = JSON.parse(fs.readFileSync(new URL('./package.json', import.meta.url), 'utf8'));
  const tauri = JSON.parse(fs.readFileSync(new URL('./src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
  const cargo = fs.readFileSync(new URL('./src-tauri/Cargo.toml', import.meta.url), 'utf8');
  const lock = fs.readFileSync(new URL('./package-lock.json', import.meta.url), 'utf8');
  const cargoLock = fs.readFileSync(new URL('./src-tauri/Cargo.lock', import.meta.url), 'utf8');
  const lib = fs.readFileSync(new URL('./src-tauri/src/lib.rs', import.meta.url), 'utf8');
  assert.equal(pkg.version, '0.1.18');
  assert.equal(tauri.version, '0.1.18');
  assert.equal(tauri.identifier, 'com.johnalindogan.slideshowpro');
  assert.match(cargo, /^version = "0\.1\.18"/m);
  assert.match(lock, /"version": "0\.1\.18"/);
  assert.match(cargoLock, /name = "slideshowpro"\nversion = "0\.1\.18"/);
  assert.equal(
    tauri.app.windows[0].additionalBrowserArgs,
    '--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --force_high_performance_gpu'
  );
  assert.match(lib, /HIGH_PERF_BROWSER_ARGS/);
  assert.match(lib, /additional_browser_args\(HIGH_PERF_BROWSER_ARGS\)/);
  assert.match(lib, /tauri_plugin_single_instance::init/);
  assert.match(lib, /api\.prevent_close\(\)/);
  assert.match(lib, /slidex:\/\/save-before-exit/);
  assert.match(lib, /fn slidex_save_done/);
  assert.doesNotMatch(lib, /window\.app_handle\(\)\.exit\(0\)/);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /id="lver"/);
  assert.match(html, /const getVersion = window\.__TAURI__ && window\.__TAURI__\.app && window\.__TAURI__\.app\.getVersion/);
  assert.match(html, /el\.textContent = String\(version\)/);
});

test('installer asks SlideX to close and never deletes app data on update', () => {
  const nsi = fs.readFileSync(new URL('./src-tauri/windows/installer.nsi', import.meta.url), 'utf8');
  assert.match(nsi, /SlideX will close to finish the update\./);
  assert.doesNotMatch(nsi, /Click OK to kill it/);
  assert.match(nsi, /taskkill\.exe" \/IM/);
  assert.match(nsi, /taskkill\.exe" \/F \/T \/IM/);
  assert.match(nsi, /\$R0 = 1/);
  assert.match(nsi, /StrCpy \$R1 "\$R1 \/UPDATE"/);
  assert.match(nsi, /EBWebView/);
  assert.match(nsi, /UPGRADEPRODUCTNAME "SlideShowX"/);
  const deleteAt = nsi.indexOf('RmDir /r "$LOCALAPPDATA\\${BUNDLEID}"');
  const guardAt = nsi.lastIndexOf('$UpdateMode <> 1', deleteAt);
  assert.ok(deleteAt !== -1 && guardAt !== -1 && deleteAt - guardAt < 500);
  assert.match(nsi, /SlideX will close to finish uninstalling\./);
  assert.ok(nsi.indexOf('!macro CloseSlideXForUpdate') < nsi.indexOf('reinst_uninstall:'));
  const macroStart = nsi.indexOf('!macro CloseSlideXForUpdate');
  const macroEnd = nsi.indexOf('!macroend', macroStart);
  const macro = nsi.slice(macroStart, macroEnd);
  assert.ok(macro.indexOf('Push $R0') < macro.indexOf('FindProcess'));
  assert.match(macro, /Pop \$R0/);
  assert.match(macro, /!define UniqueID \$\{__COUNTER__\}/);
  assert.doesNotMatch(macro, /__LINE__/);
  assert.ok(macro.indexOf('BringToFront') < macro.indexOf('Abort'));
  const labelUses = macro.match(/slidex_(?:close|wait|done|cancel)_\$\{UniqueID\}/g) || [];
  assert.ok(labelUses.length >= 4);
  const start = nsi.indexOf('reinst_uninstall:');
  const end = nsi.indexOf('reinst_done:', start);
  const block = nsi.slice(start, end);
  const closes = block.match(/!insertmacro CloseSlideXForUpdate/g) || [];
  const execs = block.match(/ExecWait '\$R1'/g) || [];
  assert.equal(closes.length, 2);
  assert.equal(execs.length, 2);
  assert.ok(block.indexOf('!insertmacro CloseSlideXForUpdate') < block.indexOf("ExecWait '$R1'"));
  const updateAt = block.indexOf('StrCpy $R1 "$R1 /UPDATE"');
  const secondClose = block.lastIndexOf('!insertmacro CloseSlideXForUpdate');
  const secondExec = block.lastIndexOf("ExecWait '$R1'");
  assert.ok(updateAt !== -1 && updateAt < secondClose && secondClose < secondExec);
  const firstClose = block.indexOf('!insertmacro CloseSlideXForUpdate');
  const firstHide = block.indexOf('HideWindow');
  const firstExec = block.indexOf("ExecWait '$R1'");
  const secondHide = block.lastIndexOf('HideWindow');
  assert.equal((block.match(/^\s*HideWindow/gm) || []).length, 2);
  assert.ok(firstClose < firstHide && firstHide < firstExec);
  assert.ok(secondClose < secondHide && secondHide < secondExec);
  const insertions = nsi.match(/!insertmacro CloseSlideXForUpdate/g) || [];
  assert.ok(insertions.length >= 2);
});

test('save before exit writes the duration and the session, then acks', () => {
  const order = [];
  const storage = {
    setItem(k, v) { order.push(k + '=' + v); }
  };
  C.saveBeforeExitThenAck(storage, 12000, { items: [{ id: 'a' }] }, () => { order.push('ack'); });
  assert.deepEqual(order, [
    'ssp_default_dur=12000',
    'ssp_autosave={"items":[{"id":"a"}]}',
    'ack'
  ]);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /function saveSessionBeforeExit/);
  assert.match(html, /slidex:\/\/save-before-exit/);
  assert.match(html, /slidex_save_done/);
  const caps = fs.readFileSync(new URL('./src-tauri/capabilities/default.json', import.meta.url), 'utf8');
  assert.match(caps, /allow-slidex-save-done/);
});

test('More tools toggles on plain d, ignores typing and modifier chords, and yields a colliding key', () => {
  const actions = [
    { id: 'mirrorH', def: 'h' },
    { id: 'moreTools', def: 'd' },
    { id: 'playlist', def: 'l' }
  ];
  assert.equal(C.resolveKeyMap(actions, null).moreTools, 'd');
  assert.equal(C.resolveKeyMap(actions, { mirrorH: 'd' }).moreTools, '');
  assert.equal(C.resolveKeyMap(actions, { moreTools: '' }).moreTools, '');
  assert.equal(C.resolveKeyMap(actions, { moreTools: 'd', playlist: 'd' }).moreTools, '');
  assert.equal(C.resolveKeyMap(actions, { moreTools: 'k' }).moreTools, 'k');
  assert.equal(C.nextMoreToolsOpen(false), true);
  assert.equal(C.nextMoreToolsOpen(true), false);
  const plain = { key: 'd', ctrlKey: false, altKey: false, metaKey: false, target: { tagName: 'DIV' } };
  assert.equal(C.moreToolsKeyFires(plain, 'd'), true);
  assert.equal(C.moreToolsKeyFires({ ...plain, key: 'D' }, 'd'), false);
  assert.equal(C.moreToolsKeyFires({ ...plain, ctrlKey: true }, 'd'), false);
  assert.equal(C.moreToolsKeyFires({ ...plain, altKey: true }, 'd'), false);
  assert.equal(C.moreToolsKeyFires({ ...plain, metaKey: true }, 'd'), false);
  assert.equal(C.moreToolsKeyFires({ ...plain, target: { tagName: 'INPUT' } }, 'd'), false);
  assert.equal(C.moreToolsKeyFires({ ...plain, target: { tagName: 'TEXTAREA' } }, 'd'), false);
  assert.equal(C.isTextEditingTarget({ tagName: 'DIV', className: 'ssp-handle', closest: () => null }), false);
  const cropInput = {
    tagName: 'INPUT',
    closest: (sel) => String(sel).includes('#cropbox') || String(sel).includes('input') ? {} : null
  };
  const renameInput = { tagName: 'INPUT', className: 'pl-rename', closest: () => null };
  assert.equal(C.isTextEditingTarget(cropInput), true);
  assert.equal(C.isTextEditingTarget(renameInput), true);
  assert.equal(C.moreToolsKeyFires({ ...plain, target: cropInput }, 'd'), false);
  assert.equal(C.moreToolsKeyFires({ ...plain, target: renameInput }, 'd'), false);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /id:'moreTools'/);
  assert.match(html, /def:'d'/);
  assert.match(html, /className = 'pl-rename'/);
  assert.match(html, /More tools \(/);
});

function close(a, b, eps = 1e-6) {
  assert.ok(Math.abs(a - b) < eps, a + ' vs ' + b);
}

test('pan delta keeps the content under the pointer for every flip and rotation', () => {
  const content = { x: 80, y: 40 };
  const dx = 12, dy = -7;
  const flips = [false, true];
  const rots = [0, 90, 180, 270];
  const cases = [];
  flips.forEach(flipH => flips.forEach(flipV => rots.forEach(rotation => {
    cases.push({ flipH, flipV, rotation, scale: 1 });
  })));
  cases.push({ flipH: true, flipV: true, rotation: 90, scale: 2.5 });
  cases.forEach(spec => {
    const base = Object.assign({ panX: 5, panY: -3, w: 200, h: 120 }, spec);
    const beforeM = C.paneContentMatrix(base);
    const before = C.applyMatrix(beforeM, content.x, content.y);
    const d = C.mapPanDelta(beforeM, dx, dy);
    const afterM = C.paneContentMatrix(Object.assign({}, base, { panX: base.panX + d.x, panY: base.panY + d.y }));
    const after = C.applyMatrix(afterM, content.x, content.y);
    close(after.x, before.x + dx);
    close(after.y, before.y + dy);
  });
});

test('zoom focus maps back to the content point on a mirrored rotated pane', () => {
  const spec = { flipH: true, flipV: false, rotation: 90, scale: 1.4, panX: 15, panY: -8, w: 300, h: 180 };
  const M = C.paneContentMatrix(spec);
  const local = { x: 70, y: 40 };
  const screen = C.applyMatrix(M, local.x, local.y);
  const back = C.mapFocusPoint(M, screen.x, screen.y);
  close(back.x, local.x);
  close(back.y, local.y);
  const main = C.paneContentMatrix({ flipH: false, flipV: false, rotation: 0, scale: 1, panX: 0, panY: 0, w: 200, h: 100 });
  const copy = C.paneContentMatrix({ flipH: true, flipV: false, rotation: 0, scale: 1, panX: 0, panY: 0, w: 200, h: 100 });
  const c = { x: 40, y: 50 };
  const fMain = C.mapFocusPoint(main, C.applyMatrix(main, c.x, c.y).x, C.applyMatrix(main, c.x, c.y).y);
  const fCopy = C.mapFocusPoint(copy, C.applyMatrix(copy, c.x, c.y).x, C.applyMatrix(copy, c.x, c.y).y);
  close(fMain.x, c.x); close(fMain.y, c.y);
  close(fCopy.x, c.x); close(fCopy.y, c.y);
  const mirror = C.paneContentMatrix({ flipH: true, w: 200, h: 100, scale: 1, rotation: 0 });
  const left = C.mapFocusPoint(mirror, 10, 50);
  close(left.x, 190);
  close(left.y, 50);
  const focus = C.zoomFocusInView(mirror, 10, 50, 200, 100, 800, 400);
  close(focus.x, 190 / 200 * 800);
  close(focus.y, 50 / 100 * 400);
});

test('arrow keys use the pane under the pointer and clamp after the inverse map', () => {
  const panes = [
    { id: 'main', x: 0, y: 0, w: 100, h: 100 },
    { id: 'maxcomp', x: 100, y: 0, w: 100, h: 100 },
    { id: 'maxcomp2', x: 200, y: 0, w: 100, h: 100 }
  ];
  const mats = {
    main: C.matIdent(),
    maxcomp: C.matScale(-1, 1),
    maxcomp2: C.matRotate(90)
  };
  assert.equal(C.paneForArrow({ x: 150, y: 10, insideWindow: true }, panes, 'main'), 'maxcomp');
  assert.equal(C.paneForArrow({ x: 250, y: 10, insideWindow: true }, panes, 'main'), 'maxcomp2');
  assert.equal(C.paneForArrow({ x: 10, y: 10, insideWindow: true }, panes, 'main'), 'main');
  assert.equal(C.paneForArrow({ x: 150, y: 400, insideWindow: true }, panes, 'main'), 'main');
  assert.equal(C.paneForArrow({ x: 150, y: 10, insideWindow: false }, panes, 'main'), 'main');
  const over = C.arrowGestureMatrix({ x: 150, y: 10, insideWindow: true }, panes, mats, 'main');
  const left = C.arrowGestureMatrix({ x: 150, y: 10, insideWindow: false }, panes, mats, 'main');
  const away = C.arrowGestureMatrix({ x: 10, y: 400, insideWindow: true }, panes, mats, 'main');
  assert.equal(over.a, -1);
  assert.equal(left.a, 1);
  assert.equal(away.a, 1);
  const view = { zoom: 2, panX: 0, panY: 0 };
  const vp = [1000, 800, 2000, 1600];
  const identNudge = C.arrowNudgeThenClamp(view, mats.main, 1, 0, ...vp);
  close(identNudge.panX, 4);
  const flipNudge = C.arrowNudgeThenClamp(view, mats.maxcomp, 1, 0, ...vp);
  close(flipNudge.panX, -4);
  const fallen = C.arrowNudgeThenClamp(view, left, 1, 0, ...vp);
  close(fallen.panX, identNudge.panX);
  const hit = C.clampedPanAfterDelta(view, C.matIdent(), 5000, 0, ...vp);
  const hitMirror = C.clampedPanAfterDelta(view, C.matScale(-1, 1), -5000, 0, ...vp);
  const hitRot = C.clampedPanAfterDelta(view, C.matRotate(90), 0, 5000, ...vp);
  close(hit.panX, hitMirror.panX);
  close(hit.panY, hitMirror.panY);
  close(hit.panX, hitRot.panX);
  close(hit.panY, hitRot.panY);
  assert.ok(hit.panX < 5000);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /new DOMMatrix\(/);
  assert.match(html, /getComputedStyle\(node\)\.transform/);
  assert.match(html, /mapPanDelta|clampedPanAfterDelta|arrowNudgeThenClamp/);
  assert.match(html, /paneForArrow/);
  assert.match(html, /insideWindow/);
  assert.doesNotMatch(html, /isMirror && !isVmax/);
  const panAt = html.indexOf('function doPan');
  const panBody = html.slice(panAt, html.indexOf('function resetPan', panAt));
  assert.ok(panBody.indexOf('arrowNudgeThenClamp') !== -1 || panBody.indexOf('mapPanDelta') !== -1);
  assert.ok(panBody.indexOf('clampManualView') > panBody.indexOf('mapPanDelta') || panBody.includes('arrowNudgeThenClamp'));
});

function coverGap(panX, panY, vpW, vpH, contentW, contentH, rotation) {
  const zoomed = C.placedRect({ zoom: 3.2, panX: 0, panY: 0 }, vpW, vpH, contentW, contentH, rotation);
  const boxW = zoomed.w;
  const boxH = zoomed.h;
  const rot = ((rotation % 360) + 360) % 360;
  const quarter = rot === 90 || rot === 270;
  const visW = quarter ? boxH : boxW;
  const visH = quarter ? boxW : boxH;
  let ox = panX;
  let oy = panY;
  if (rot === 90) { ox = -panY; oy = panX; }
  else if (rot === 270) { ox = panY; oy = -panX; }
  else if (rot === 180) { ox = -panX; oy = -panY; }
  const left = vpW / 2 + ox - visW / 2;
  const top = vpH / 2 + oy - visH / 2;
  const right = left + visW;
  const bottom = top + visH;
  return {
    gapX: visW + 0.5 >= vpW ? Math.max(0, left, vpW - right) : 0,
    gapY: visH + 0.5 >= vpH ? Math.max(0, top, vpH - bottom) : 0
  };
}

test('rotated pan limits cover the viewport in every layout, flip, and quarter turn', () => {
  const stage = { w: 1600, h: 1000 };
  const content = { w: 1080, h: 1920 };
  const layouts = ['single', 'hmax3', 'vmax', 'vmax3'];
  const flips = [false, true];
  const turns = [0, 90];
  const corners = [[1, 1], [1, -1], [-1, 1], [-1, -1]];
  layouts.forEach(layout => {
    const vp = C.layoutViewport(layout, stage.w, stage.h);
    flips.forEach(flipH => flips.forEach(flipV => turns.forEach(rotation => {
      corners.forEach(sign => {
        const view = { zoom: 3.2, panX: sign[0] * 1e9, panY: sign[1] * 1e9 };
        const orient = { flipH, flipV, rotation };
        const clamped = C.clampOrientedView(view, vp.w, vp.h, content.w, content.h, orient);
        const plain = C.clampOrientedView(view, vp.w, vp.h, content.w, content.h, { rotation });
        close(clamped.panX, plain.panX);
        close(clamped.panY, plain.panY);
        const gap = coverGap(clamped.panX, clamped.panY, vp.w, vp.h, content.w, content.h, rotation);
        assert.ok(gap.gapX < 1 && gap.gapY < 1, layout + ' H' + flipH + ' V' + flipV + ' r' + rotation + ' ' + sign);
      });
    })));
  });
  const raw = C.clampView({ zoom: 3.2, panX: 1e9, panY: 1e9 }, stage.w, stage.h, content.w, content.h, 0);
  const black = coverGap(raw.panX, raw.panY, stage.w, stage.h, content.w, content.h, 90);
  assert.ok(Math.max(black.gapX, black.gapY) > 250);
  const fixed = C.clampOrientedView(
    { zoom: 3.2, panX: 1e9, panY: 1e9 }, stage.w, stage.h, content.w, content.h, { rotation: 90, flipH: true, flipV: true }
  );
  const flush = coverGap(fixed.panX, fixed.panY, stage.w, stage.h, content.w, content.h, 90);
  assert.ok(flush.gapX < 1 && flush.gapY < 1);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /clampOrientedView/);
});

function screenOfContent(view, relX, relY, vpW, vpH, contentW, contentH, orient) {
  const rect = C.placedRect(view, vpW, vpH, contentW, contentH, orient.rotation || 0);
  const M = C.orientationMatrix(vpW, vpH, orient);
  return C.applyMatrix(M, rect.x + relX * rect.w, rect.y + relY * rect.h);
}

test('scroll zoom keeps the point under the pointer within 1px for H, V, and 90', () => {
  const vpW = 1600, vpH = 1000, cw = 1080, ch = 1920;
  const orients = [
    { flipH: true, flipV: false, rotation: 0 },
    { flipH: false, flipV: true, rotation: 0 },
    { flipH: false, flipV: false, rotation: 90 },
    { flipH: true, flipV: false, rotation: 90 }
  ];
  orients.forEach(orient => {
    const view = C.clampOrientedView({ zoom: 4, panX: 30, panY: -20 }, vpW, vpH, cw, ch, orient);
    const relX = 0.37, relY = 0.58;
    const before = screenOfContent(view, relX, relY, vpW, vpH, cw, ch, orient);
    const next = C.zoomAtPointer(view, 1.15, before.x, before.y, vpW, vpH, cw, ch, orient);
    const after = screenOfContent(next, relX, relY, vpW, vpH, cw, ch, orient);
    close(after.x, before.x, 1);
    close(after.y, before.y, 1);
  });
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const focusAt = html.indexOf('function focusForZoom');
  const focus = html.slice(focusAt, html.indexOf('function stopCompanionMotion', focusAt));
  assert.match(focus, /mapFocusPoint/);
  const zoomAt = html.indexOf('function changeZoom');
  const zoom = html.slice(zoomAt, html.indexOf('let pointerOver', zoomAt));
  assert.match(zoom, /zoomToward\(/);
});

test('closing the start screen leaves the saved session byte-identical', () => {
  const saved = '{"sspVersion":1,"savedAt":1,"items":[{"id":"a","type":"image","name":"a.jpg"}]}';
  const data = { ssp_autosave: saved };
  const storage = {
    getItem(k) { return Object.prototype.hasOwnProperty.call(data, k) ? data[k] : null; },
    setItem(k, v) { data[k] = String(v); }
  };
  C.saveBeforeExitThenAck(storage, 10000, { items: [] }, () => {}, { loaded: false, dirty: false });
  assert.equal(storage.getItem('ssp_autosave'), saved);
  assert.equal(C.shouldSaveSessionOnClose({ loaded: false, dirty: false, session: { items: [] } }), false);
  assert.equal(C.shouldSaveSessionOnClose({ loaded: true, dirty: false, session: { items: [] } }), false);
  assert.equal(C.shouldSaveSessionOnClose({ loaded: true, dirty: false, session: { items: [{ id: 'a' }] } }), true);
  assert.equal(C.shouldSaveSessionOnClose({ loaded: false, dirty: true, session: { items: [{ id: 'a' }] } }), true);
  assert.equal(C.shouldSaveSessionOnClose({ loaded: false, dirty: false, session: { items: [{ id: 'a' }] } }), false);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /shouldSaveSessionOnClose/);
  assert.match(html, /sessionLoaded/);
});

test('cancel leaves shortcuts, playlists, and locate paths unchanged', () => {
  const pending = Promise.resolve(true);
  assert.equal(C.confirmMeansYes(pending), false);
  assert.equal(C.confirmMeansYes(false), false);
  assert.equal(C.confirmMeansYes(true), true);
  const map = { rotCW: 'x', moreTools: 'q' };
  const defaults = { rotCW: 'r', moreTools: 'd' };
  assert.equal(C.resetShortcutsIfConfirmed(false, map, defaults), map);
  assert.equal(C.resetShortcutsIfConfirmed(pending, map, defaults), map);
  const reset = C.resetShortcutsIfConfirmed(true, map, defaults);
  assert.equal(reset.rotCW, 'r');
  assert.equal(reset.moreTools, 'd');
  assert.notEqual(reset, map);

  const store = [{ name: 'Trip', json: '{"items":[1]}' }];
  const kept = C.namedPlaylistsAfterReplace(store, 'Trip', '{"items":[2]}', false, true);
  assert.equal(kept[0].json, '{"items":[1]}');
  assert.equal(kept.length, 1);
  const replaced = C.namedPlaylistsAfterReplace(store, 'Trip', '{"items":[2]}', true, true);
  assert.equal(replaced[0].json, '{"items":[2]}');
  assert.equal(store[0].json, '{"items":[1]}');

  const names = ['Trip', 'Home'];
  assert.deepEqual(C.namedPlaylistsAfterDelete(names, 'Trip', false), names);
  assert.deepEqual(C.namedPlaylistsAfterDelete(names, 'Trip', pending), names);
  assert.deepEqual(C.namedPlaylistsAfterDelete(names, 'Trip', true), ['Home']);

  const items = [{ path: 'C:\\old\\a.jpg', missing: true }, { path: 'C:\\keep\\b.jpg', missing: false }];
  const rewrites = { 'C:\\old\\a.jpg': 'D:\\new\\a.jpg' };
  const cancelled = C.itemsAfterLocateConfirm(items, rewrites, false);
  assert.equal(cancelled[0].path, items[0].path);
  assert.equal(cancelled[0].missing, true);
  assert.equal(cancelled[1].path, items[1].path);
  const applied = C.itemsAfterLocateConfirm(items, rewrites, true);
  assert.equal(applied[0].path, 'D:\\new\\a.jpg');
  assert.equal(applied[0].missing, false);
  assert.equal(applied[1].path, items[1].path);
  assert.equal(items[0].path, 'C:\\old\\a.jpg');

  assert.equal(C.missingFilesNotice(3), "3 files couldn't be found");
  assert.equal(C.missingFilesNotice(1), "1 file couldn't be found");
  assert.equal(C.missingFilesNotice(0), '');
});

test('HTML and JS have no bare confirm() and the four prompts await a dialog', () => {
  const bare = /(?<![\w$.])confirm\s*\(/;
  ['./SlideShowPro.html', './slidex-core.js', './demo/SlideShowPro.html'].forEach(rel => {
    const text = fs.readFileSync(new URL(rel, import.meta.url), 'utf8');
    assert.equal(bare.test(text), false, rel);
  });
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /async function sspConfirm/);
  assert.match(html, /dialog\.confirm\(/);
  assert.match(html, /window\.confirm\(message\)/);
  const resetAt = html.indexOf("g('kedresetall').onclick");
  const reset = html.slice(resetAt, html.indexOf("g('kedsearch')", resetAt));
  assert.match(reset, /await sspConfirm\(/);
  assert.ok(reset.indexOf('await sspConfirm') < reset.indexOf('keyMap[f.id]'));
  const save = html.slice(html.indexOf('async function saveNamedPlaylistFlow'), html.indexOf('function closePlaylistLibrary'));
  assert.match(save, /await sspConfirm\(/);
  assert.ok(save.indexOf('await sspConfirm') < save.indexOf('save_named_playlist'));
  const delAt = html.indexOf('del.onclick = async');
  const del = html.slice(delAt, html.indexOf('btns.appendChild(load)', delAt));
  assert.match(del, /await sspConfirm\(/);
  assert.ok(del.indexOf('await sspConfirm') < del.indexOf('delete_named_playlist'));
  const locate = html.slice(html.indexOf('async function locateMissingFolder'), html.indexOf('function bindFramingExtras'));
  assert.match(locate, /await sspConfirm\(/);
  assert.ok(locate.indexOf('await sspConfirm') < locate.indexOf('register_allowed_paths'));
  assert.ok(locate.indexOf('items[idx] = hit') < locate.indexOf('autoSave()'));
  assert.ok(locate.indexOf('autoSave()') < locate.lastIndexOf('showToast'));
  const caps = fs.readFileSync(new URL('./src-tauri/capabilities/default.json', import.meta.url), 'utf8');
  assert.match(caps, /dialog:allow-ask/);
  assert.match(caps, /dialog:allow-confirm/);
  assert.match(html, /missingFilesNotice/);
  assert.match(html, /planSessionRestore/);
  assert.match(html, /async function restoreTauriSession/);
  assert.match(html, /toggleTimer/);
});

test('the Cast panel can copy the Cast log path', () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /id="castLog"/);
  assert.match(html, /Copy Cast log path/);
  assert.match(html, /cast_log_path/);
  const caps = fs.readFileSync(new URL('./src-tauri/capabilities/default.json', import.meta.url), 'utf8');
  assert.match(caps, /allow-cast-log-path/);
  const perms = fs.readFileSync(new URL('./src-tauri/permissions/launch.toml', import.meta.url), 'utf8');
  assert.match(perms, /allow-cast-log-path/);
  assert.match(perms, /cast_log_path/);
  assert.match(perms, /cast_log_dur_fallback/);
  assert.match(caps, /allow-cast-log-dur-fallback/);
});

test('a cast skip message names TV limits and a level the TV may not play', () => {
  assert.equal(C.isCastSkipMessage('Preparing for TV…'), true);
  assert.equal(C.isCastSkipMessage("90 fps, 2560x2560 and 245 Mbit/s aren't supported by the TV"), true);
  assert.equal(C.isCastSkipMessage("10-bit isn't supported by the TV"), true);
  assert.equal(C.isCastSkipMessage('H.264 level 5.2 may not play on this TV'), true);
  assert.equal(C.isCastSkipMessage('HEVC level 5.2 may not play on this TV'), true);
  assert.equal(C.isCastSkipMessage('Unsupported media. Cast plays JPEG, PNG, and MP4 (H.264 + AAC). Skipped.'), true);
  assert.equal(C.isCastSkipMessage("The TV can't play this MKV as-is."), true);
  assert.equal(C.isCastSkipMessage("The TV can't play this HEVC as-is."), true);
  assert.equal(C.isCastSkipMessage('This video is still being written, or it has no moov index yet.'), true);
  assert.equal(C.isCastSkipMessage("Couldn't check this video (file is missing)."), true);
  assert.equal(C.isCastSkipMessage('The TV copy failed.'), true);
  assert.equal(C.isCastSkipMessage('TV unreachable. It may be off or the Wi-Fi dropped.'), false);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const load = html.slice(html.indexOf('function loadCurrent'), html.indexOf('function syncTransport'));
  assert.match(load, /castSkipMessage\(msg\)/);
  assert.match(load, /rememberCastBad\(path\)/);
  assert.match(html, /__sspCastBadPaths/);
});

test('mkv, webm, and mp4 names are kept so cast can verify them', () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const src = html.slice(html.indexOf('const IMAGE_EXTS'), html.indexOf('function normalizeMediaPathEntry'));
  const kind = vm.runInNewContext(`${src}
    ({
      mkv: mediaKindFromName('clip.mkv'),
      webm: mediaKindFromName('clip.webm'),
      avi: mediaKindFromName('clip.avi'),
      mp4: mediaKindFromName('cut.mp4'),
      txt: mediaKindFromName('notes.txt'),
      mkvFile: isAcceptedMediaFile({ name: 'clip.mkv', type: '' }),
      webmFile: isAcceptedMediaFile({ name: 'clip.webm', type: '' })
    })`, { String });
  assert.equal(kind.mkv, 'video');
  assert.equal(kind.webm, 'video');
  assert.equal(kind.avi, 'video');
  assert.equal(kind.mp4, 'video');
  assert.equal(kind.txt, null);
  assert.equal(kind.mkvFile, true);
  assert.equal(kind.webmFile, true);
  assert.match(html, /\.mkv/);
});

test('a cast skip during loadCurrent moves straight to the next slide', async () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const src = html.slice(html.indexOf('function currentSkipped'), html.indexOf('function syncTransport'));
  const moves = [];
  const timers = [];
  const core = {
    isCastSkipMessage: C.isCastSkipMessage,
    DEFAULT_IMAGE_DURATION_MS: C.DEFAULT_IMAGE_DURATION_MS
  };
  const sandbox = {
    Promise,
    String,
    Number,
    Math,
    Object,
    window: { __sspCastActive: true, SlideXCore: core },
    SlideXCore: core,
    castActive: true,
    skipBurst: 0,
    skippedNotes: [],
    cur: 0,
    setTimeout(fn, ms) { timers.push(ms); return 0; },
    setStatus() {},
    showToast() {},
    esc(s) { return String(s); },
    sspCastCurrentPath() { return 'C:/slides/bad.mp4'; },
    sspCastCurrentName() { return 'bad.mp4'; },
    sspCastPaths() { return ['C:/slides/a.jpg', 'C:/slides/bad.mp4', 'C:/slides/c.jpg']; },
    sspCastPlaying() { return true; },
    nextMI() { return 1; },
    goTo(index, source) { moves.push([index, source]); },
    call(cmd) {
      assert.equal(cmd, 'cast_load');
      return Promise.reject(new Error("90 fps isn't supported by the TV"));
    }
  };
  const done = vm.runInNewContext(src + '\nloadCurrent("user");', sandbox);
  await done;
  assert.deepEqual(moves, [[1, 'user']]);
  assert.deepEqual(timers, []);
  assert.equal(sandbox.window.__sspCastBadPaths['C:/slides/bad.mp4'], true);
  assert.equal(sandbox.skipBurst, 1);
  assert.equal(sandbox.cur, 0);
});

test('Preparing for TV moves on without marking the item bad', async () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const src = html.slice(html.indexOf('function currentSkipped'), html.indexOf('function syncTransport'));
  const caps = fs.readFileSync(new URL('./src-tauri/capabilities/default.json', import.meta.url), 'utf8');
  const perms = fs.readFileSync(new URL('./src-tauri/permissions/launch.toml', import.meta.url), 'utf8');
  const nsi = fs.readFileSync(new URL('./src-tauri/windows/installer.nsi', import.meta.url), 'utf8');
  const pin = JSON.parse(fs.readFileSync(new URL('./src-tauri/ffmpeg.pin.json', import.meta.url), 'utf8'));
  const tauri = JSON.parse(fs.readFileSync(new URL('./src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
  assert.equal(pin.version, '9.0.2');
  assert.equal(pin.archive_sha256, 'f0e46253c70dfe902bac915dfb4224f0cf1b7c6eeab9da2ccc9a5581f9a71b13');
  assert.equal(pin.exe_sha256, '589e50b766d251afdf181dd664d40bd94407e200b019989fd7468c7d118a28d0');
  assert.equal(tauri.bundle.resources['resources/ffmpeg/ffmpeg.exe'], 'ffmpeg.exe');
  assert.equal(tauri.bundle.resources['resources/ffmpeg/LICENSE'], 'ffmpeg-license.txt');
  assert.match(html, /id="castEncoder"/);
  assert.match(html, /value="libx264"/);
  assert.match(html, /value="h264_mf"/);
  assert.match(html, /value="h264_nvenc"/);
  assert.match(html, /value="h264_qsv"/);
  assert.match(html, /cast_get_tv_encoder/);
  assert.match(html, /cast_set_tv_encoder/);
  assert.match(caps, /allow-cast-get-tv-encoder/);
  assert.match(caps, /allow-cast-set-tv-encoder/);
  assert.match(perms, /cast_get_tv_encoder/);
  assert.match(perms, /cast_set_tv_encoder/);
  assert.doesNotMatch(nsi, /ffmpeg\.exe/);
  assert.equal(
    C.castBannerText({ player_state: 'playing', error: 'Preparing for TV…' }, 'clip.mkv'),
    'Preparing for TV…'
  );
  function runLoad(paths, rejectMessage) {
    const moves = [];
    const statuses = [];
    const core = {
      isCastSkipMessage: C.isCastSkipMessage,
      DEFAULT_IMAGE_DURATION_MS: C.DEFAULT_IMAGE_DURATION_MS
    };
    const sandbox = {
      Promise,
      String,
      Number,
      Math,
      Object,
      window: { __sspCastActive: true, SlideXCore: core },
      SlideXCore: core,
      castActive: true,
      skipBurst: 0,
      skippedNotes: [],
      cur: 0,
      setTimeout() { return 0; },
      setStatus(msg) { statuses.push(msg); },
      showToast() {},
      esc(s) { return String(s); },
      sspCastCurrentPath() { return paths[0]; },
      sspCastCurrentName() { return 'clip.mkv'; },
      sspCastPaths() { return paths; },
      sspCastPlaying() { return true; },
      nextMI() { return 1; },
      goTo(index, source) { moves.push([index, source]); },
      call() { return Promise.reject(new Error(rejectMessage)); }
    };
    return vm.runInNewContext(src + '\nloadCurrent("user");', sandbox).then(() => ({ moves, statuses, sandbox }));
  }
  const moved = await runLoad(
    ['C:/slides/clip.mkv', 'C:/slides/next.jpg'],
    'Preparing for TV…'
  );
  assert.deepEqual(moved.moves, [[1, 'user']]);
  assert.equal(moved.sandbox.window.__sspCastBadPaths, undefined);
  assert.equal(moved.statuses[0], 'Preparing for TV…');
  assert.equal(moved.sandbox.skipBurst, 1);
  const stayed = await runLoad(['C:/slides/clip.mkv'], 'Preparing for TV…');
  assert.deepEqual(stayed.moves, []);
  assert.equal(stayed.sandbox.window.__sspCastBadPaths, undefined);
  assert.equal(stayed.statuses[0], 'Preparing for TV…');
  assert.equal(stayed.sandbox.skipBurst, 0);
});

test('a cast ignores local video events and tags the command source', () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /window\.__sspCastActive/);
  assert.match(html, /window\.__sspCastGen/);
  assert.match(html, /stopImmediatePropagation/);
  assert.match(html, /function sspCastHook\(name, source\)/);
  assert.match(html, /goTo\(nextMI\(\), 'timer'\)/);
  const sched = html.slice(html.indexOf('function schedNext()'), html.indexOf('function goTo('));
  assert.match(sched, /if \(!playing \|\| isV\(cur\)\) return/);
  assert.match(html, /ev === "finished"/);
  assert.match(html, /else if \(ev === "finished" && sspCastPlaying\(\)\)/);
  assert.match(html, /loaded > 0 && loaded - 1 !== cur\) goTo\(loaded - 1, 'remote'\)/);
  assert.doesNotMatch(html, /__sspCastMoved/);
  assert.match(html, /ev === "load_failed"/);
  const failed = html.slice(html.indexOf('ev === "load_failed"'), html.indexOf('ev === "paused"'));
  assert.doesNotMatch(failed, /goTo\(/);
  const finished = html.slice(html.indexOf('ev === "finished"'), html.indexOf('if (!field(st, "connected"))'));
  assert.match(finished, /sspCastPlaying\(\)/);
  assert.match(finished, /__sspCastEcho = true/);
  assert.doesNotMatch(finished, /__sspCastMoved/);
  assert.match(finished, /loaded - 1 !== cur/);
  assert.match(finished, /goTo\(loaded - 1, 'remote'\)/);
  assert.match(finished, /item_index/);
  assert.match(html, /sspCastHook\('transport', window\.__sspCastEcho \? 'remote' : 'user'\)/);
  assert.match(html, /sspCastHook\('slide', source \|\| 'user'\)/);
  assert.match(html, /source: src/);
  const play = html.slice(html.indexOf('async function playCurVid'), html.indexOf('function showSlide'));
  assert.match(play, /if \(window\.__sspCastActive\) return/);
  const ended = html.slice(html.indexOf('med.onended'), html.indexOf('return sl;'));
  assert.match(ended, /if \(window\.__sspCastActive\) return/);
  const disc = html.slice(html.indexOf('castDisconnect'), html.indexOf('setInterval(watchNetwork'));
  assert.match(disc, /stopLocalCast\(\)/);
  assert.match(disc, /cast_disconnect/);
});

test('the cast banner follows status immediately and shows Paused', () => {
  assert.equal(C.castBannerText({ player_state: 'paused' }, 'clip.mp4'), 'Paused');
  assert.equal(C.castBannerText({ player_state: 'PAUSED', error: '' }, 'clip.mp4'), 'Paused');
  assert.equal(C.castBannerText({ player_state: 'playing' }, 'clip.mp4'), 'Casting clip.mp4.');
  assert.equal(C.castBannerText({ player_state: 'playing' }, ''), 'Casting the current slide.');
  assert.equal(
    C.castBannerText({ player_state: 'playing', error: '90 fps isn\'t supported by the TV' }, 'clip.mp4'),
    '90 fps isn\'t supported by the TV'
  );
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const load = html.slice(html.indexOf('function loadCurrent'), html.indexOf('function syncTransport'));
  const sync = html.slice(html.indexOf('function syncTransport'), html.indexOf('function poll'));
  const poll = html.slice(html.indexOf('function poll()'), html.indexOf('window.__sspCast = {'));
  assert.match(load, /refreshCastBanner\(\)/);
  assert.doesNotMatch(load, /setTimeout/);
  assert.match(sync, /refreshCastBanner\(\)/);
  assert.doesNotMatch(sync, /setTimeout/);
  assert.match(poll, /applyCastBanner\(st\)/);
  assert.doesNotMatch(poll, /setTimeout/);
  assert.match(html, /SlideXCore\.castBannerText/);
});

test('the missing firewall rules message is shown before the panel opens', async () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const sentence = 'No Private inbound rule for slideshowpro.exe. Cast needs TCP 47200-47215 and UDP 5353 on private networks. Re-run the installer and accept the Windows prompt.';
  const markup = html.slice(html.indexOf('id="castFirewall"'), html.indexOf('id="castFirewall"') + 400);
  assert.match(markup, new RegExp(sentence.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  const refresh = html.slice(html.indexOf('function refreshFirewall'), html.indexOf('function watchNetwork'));
  const catchBody = refresh.slice(refresh.indexOf('.catch'));
  assert.doesNotMatch(catchBody, /textContent/);
  assert.match(refresh, /\.catch\(function \(\) \{\s*\}\)/);
  assert.match(refresh, /status=match/);
  assert.match(refresh, /status=present/);
  const sandbox = {
    Promise,
    firewallEl: { textContent: sentence },
    call() {
      return Promise.resolve({
        state: 'added',
        detail: 'Private firewall rules are installed for slideshowpro.exe: TCP 47200-47215 and UDP 5353.'
      });
    }
  };
  vm.runInNewContext(refresh + '\nrefreshFirewall();', sandbox);
  await Promise.resolve();
  assert.equal(
    sandbox.firewallEl.textContent,
    'Private firewall rules are installed for slideshowpro.exe: TCP 47200-47215 and UDP 5353.'
  );
  sandbox.firewallEl.textContent = sentence;
  sandbox.call = () => Promise.resolve({ state: 'present', detail: '' });
  vm.runInNewContext(refresh + '\nrefreshFirewall();', sandbox);
  await Promise.resolve();
  assert.equal(sandbox.firewallEl.textContent, '');
  sandbox.firewallEl.textContent = sentence;
  sandbox.call = () => Promise.resolve({ state: 'match', detail: 'status=match' });
  vm.runInNewContext(refresh + '\nrefreshFirewall();', sandbox);
  await Promise.resolve();
  assert.equal(sandbox.firewallEl.textContent, '');
  sandbox.firewallEl.textContent = sentence;
  sandbox.call = () => Promise.reject(new Error('query failed'));
  vm.runInNewContext(refresh + '\nrefreshFirewall();', sandbox);
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(sandbox.firewallEl.textContent, sentence);
});

test('the TV unreachable banner comes only from cast_status', () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const load = html.slice(html.indexOf('function loadCurrent'), html.indexOf('function syncTransport'));
  const sync = html.slice(html.indexOf('function syncTransport'), html.indexOf('function poll'));
  const poll = html.slice(html.indexOf('if (!field(st, "connected"))'), html.indexOf('window.__sspCast = {'));
  assert.doesNotMatch(load, /stopLocalCast/);
  assert.doesNotMatch(load, /unreachable/);
  assert.doesNotMatch(sync, /stopLocalCast/);
  assert.doesNotMatch(sync, /unreachable/);
  assert.match(poll, /if \(!field\(st, "connected"\)\)/);
  assert.match(poll, /stopLocalCast\(\)/);
  assert.match(poll, /TV unreachable/);
});

test('a photo timer then a video FINISHED leaves cur on item 3', () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const goToSrc = html.slice(html.indexOf('function goTo('), html.indexOf('function updPlayBtn'));
  const finishedSrc = html.slice(
    html.indexOf('else if (ev === "finished" && sspCastPlaying())'),
    html.indexOf('if (!field(st, "connected"))')
  );
  const sandbox = {
    console,
    clearTimeout,
    clearInterval,
    setTimeout,
    cancelAnimationFrame() {},
    cur: 0,
    playing: true,
    tmr: null,
    ptmr: null,
    kbId: null,
    compKbId: null,
    kbManualHold: false,
    transitionType: 'none',
    sls: [],
    stage: { classList: { remove() {} } },
    getMediaItems() {
      return [
        { name: 'one.jpg', path: '/p/one.jpg' },
        { name: 'two.mp4', path: '/p/two.mp4' },
        { name: 'three.jpg', path: '/p/three.jpg' },
      ];
    },
    isV(i) { return i === 1; },
    remountStageWindow() {},
    getSl() { return null; },
    sspCastHook() {},
    applySlideSettings() {},
    scrollStripTo() {},
    updStripActive() {},
    g() { return { textContent: '' }; },
    pauseVids() {},
    showMaxCompanion() {},
    staticT() {},
    startKB() {},
    startProg() {},
    schedNext() {},
    updPlCur() {},
    scheduleStagePrune() {},
    notifyPlaylistSync() {},
    playCurVid() {},
    startVProg() {},
    window: { __sspCastActive: true },
  };
  vm.runInNewContext(`
    function field(obj, name) {
      if (!obj) return undefined;
      if (obj[name] != null) return obj[name];
      return undefined;
    }
    function sspCastPlaying() { return !!playing; }
    ${goToSrc}
    function applyFinished(st) {
      var ev = field(st, "remote_event");
      if (false) {}
      ${finishedSrc}
    }
  `, sandbox);
  sandbox.goTo(1, 'timer');
  assert.equal(sandbox.cur, 1);
  sandbox.applyFinished({ remote_event: 'finished', item_index: 3 });
  assert.equal(sandbox.cur, 2);
});

function whatsappShots(n) {
  const items = [];
  for (let i = 1; i <= n; i++) {
    const name = `WhatsApp Image 2026-10-14 at 09.0${i}.0${i}.jpg`;
    items.push({
      id: `wa-${i}`,
      type: 'image',
      name,
      path: `C:\\Users\\John\\Pictures\\${name}`
    });
  }
  return items;
}

function shotNames(items) {
  return items.map(it => it.name);
}

const pageHtml = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
const tauriConf = JSON.parse(fs.readFileSync(new URL('./src-tauri/tauri.conf.json', import.meta.url), 'utf8'));

test('reordering by handle works', () => {
  const shots = whatsappShots(8);
  const rows = C.stackMediaRows(shots.length);
  assert.equal(C.pointerPassedReorderThreshold(2, 2), false);
  assert.equal(pageHtml.includes('pointerPassedReorderThreshold'), true);
  assert.equal(pageHtml.includes('reorderByHandle'), true);
  assert.equal(pageHtml.includes("addEventListener('dragstart'"), false);
  assert.equal(pageHtml.includes('div.draggable = true'), false);
  const below = C.reorderByHandle({
    items: shots,
    rows,
    fromItemsIdx: 0,
    clientY: rows[0].top + 2,
    listTop: 0,
    scrollTop: 0,
    selected: []
  });
  assert.equal(below.moved, false);
  assert.deepEqual(Array.from(shotNames(below.items)), shotNames(shots));
  const moved = C.reorderByHandle({
    items: shots,
    rows,
    fromItemsIdx: 0,
    clientY: rows[2].top + rows[2].height * 0.75,
    listTop: 0,
    scrollTop: 0,
    selected: []
  });
  assert.equal(moved.moved, true);
  assert.equal(moved.hit.after, true);
  assert.equal(moved.hit.itemsIdx, 2);
  assert.deepEqual(Array.from(shotNames(moved.items)), [
    shots[1].name, shots[2].name, shots[0].name,
    shots[3].name, shots[4].name, shots[5].name, shots[6].name, shots[7].name
  ]);
  const multi = C.reorderByHandle({
    items: shots,
    rows,
    fromItemsIdx: 0,
    clientY: rows[7].top + rows[7].height * 0.75,
    selected: [0, 1]
  });
  assert.equal(multi.clearedSelection, true);
  assert.deepEqual(Array.from(shotNames(multi.items)), [
    shots[2].name, shots[3].name, shots[4].name, shots[5].name, shots[6].name, shots[7].name,
    shots[0].name, shots[1].name
  ]);
});

test('reordering by strip works', () => {
  const shots = whatsappShots(8);
  assert.equal(pageHtml.includes('reorderByStrip'), true);
  const leftOfFirst = C.STRIP_PAD_X + C.STRIP_THUMB_W * 0.25;
  const moved = C.reorderByStrip({
    items: shots,
    fromMediaIndex: 7,
    clientX: leftOfFirst,
    stripLeft: 0,
    scrollLeft: 0
  });
  assert.equal(moved.moved, true);
  assert.equal(moved.hit.mediaIndex, 0);
  assert.equal(moved.hit.after, false);
  assert.deepEqual(Array.from(shotNames(moved.items)), [
    shots[7].name, shots[0].name, shots[1].name, shots[2].name,
    shots[3].name, shots[4].name, shots[5].name, shots[6].name
  ]);
  const withFolder = [
    { type: 'folder', id: 'f', name: 'Trip' },
    shots[0], shots[1]
  ];
  const folderMove = C.reorderByStrip({
    items: withFolder,
    fromMediaIndex: 1,
    clientX: C.STRIP_PAD_X + C.STRIP_THUMB_W * 0.25,
    stripLeft: 0,
    scrollLeft: 0
  });
  assert.deepEqual(Array.from(folderMove.items.map(it => it.name)), ['Trip', shots[1].name, shots[0].name]);
  assert.deepEqual(Array.from(C.playbackNames(folderMove.items)), [shots[1].name, shots[0].name]);
});

function missingFile() {
  return {
    id: 'gone',
    type: 'image',
    name: 'missing.jpg',
    path: 'C:\\Users\\John\\Pictures\\missing.jpg',
    missing: true,
    unavailable: true
  };
}

// [s0, missing, s1, s2, s3]. selectedItems is the Media Manager mi, which skips
// the missing file, so [1, 2] is s1 and s2. Dropping them after s3 leaves the
// missing file where it is.
function playlistPastUnavailable(shots) {
  return [shots[0], missingFile(), shots[1], shots[2], shots[3]];
}

function rowsForPlaylist(items) {
  const h = C.PL_MEDIA_IMAGE_H;
  return items.map((_, i) => ({
    itemsIdx: i,
    top: C.PL_PAD_Y + i * h,
    height: h
  }));
}

test('multi-select drag by the list handle moves past one unavailable item', () => {
  const shots = whatsappShots(4);
  const items = playlistPastUnavailable(shots);
  const rows = rowsForPlaylist(items);
  const moved = C.reorderByHandle({
    items,
    rows,
    fromItemsIdx: 2,
    clientY: rows[4].top + rows[4].height * 0.75,
    selected: [1, 2]
  });
  assert.equal(moved.moved, true);
  assert.equal(moved.clearedSelection, true);
  assert.deepEqual(Array.from(shotNames(moved.items)), [
    shots[0].name, 'missing.jpg', shots[3].name, shots[1].name, shots[2].name
  ]);
  assert.equal(moved.items[1].unavailable, true);
  assert.equal(moved.items[1], items[1]);
});

test('multi-select drag by the strip moves past one unavailable item', () => {
  const shots = whatsappShots(4);
  const items = playlistPastUnavailable(shots);
  assert.deepEqual(Array.from(C.playbackNames(items)), [
    shots[0].name, shots[1].name, shots[2].name, shots[3].name
  ]);
  const cell = C.STRIP_THUMB_W + C.STRIP_GAP;
  const clientX = C.STRIP_PAD_X + 3 * cell + C.STRIP_THUMB_W / 2;
  const moved = C.reorderByStrip({
    items,
    fromMediaIndex: 1,
    clientX,
    stripLeft: 0,
    scrollLeft: 0,
    selected: [1, 2]
  });
  assert.equal(moved.moved, true);
  assert.equal(moved.hit.mediaIndex, 3);
  assert.equal(moved.hit.after, true);
  assert.equal(moved.clearedSelection, true);
  assert.deepEqual(Array.from(shotNames(moved.items)), [
    shots[0].name, 'missing.jpg', shots[3].name, shots[1].name, shots[2].name
  ]);
  assert.equal(moved.items[1].unavailable, true);
  assert.match(
    pageHtml,
    /function getMediaItems\(\) \{ return items\.filter\(it => it\.type !== 'folder' && !it\.unavailable && !it\.missing\); \}/
  );
});

test('a single drag with nothing selected keeps the slide on screen', () => {
  const shots = whatsappShots(8);
  const rows = C.stackMediaRows(shots.length);
  const beforeCur = 3;
  const cell = C.STRIP_THUMB_W + C.STRIP_GAP;
  const byHandle = C.reorderByHandle({
    items: shots,
    rows,
    fromItemsIdx: 1,
    clientY: rows[2].top + rows[2].height * 0.75,
    listTop: 0,
    scrollTop: 0,
    selected: []
  });
  assert.equal(byHandle.moved, true);
  assert.equal(byHandle.clearedSelection, false);
  const handleCur = C.playbackIndexAfterReorder(shots, beforeCur, byHandle.items);
  assert.equal(C.playbackMedia(byHandle.items)[handleCur].name, shots[beforeCur].name);
  const handleSelf = C.reorderByHandle({
    items: shots,
    rows,
    fromItemsIdx: beforeCur,
    clientY: rows[4].top + rows[4].height * 0.75,
    listTop: 0,
    scrollTop: 0,
    selected: []
  });
  assert.equal(handleSelf.moved, true);
  const handleSelfCur = C.playbackIndexAfterReorder(shots, beforeCur, handleSelf.items);
  assert.equal(C.playbackMedia(handleSelf.items)[handleSelfCur].name, shots[beforeCur].name);
  const byStrip = C.reorderByStrip({
    items: shots,
    fromMediaIndex: 1,
    clientX: C.STRIP_PAD_X + 2 * cell + C.STRIP_THUMB_W * 0.75,
    stripLeft: 0,
    scrollLeft: 0,
    selected: []
  });
  assert.equal(byStrip.moved, true);
  assert.equal(byStrip.clearedSelection, false);
  const stripCur = C.playbackIndexAfterReorder(shots, beforeCur, byStrip.items);
  assert.equal(C.playbackMedia(byStrip.items)[stripCur].name, shots[beforeCur].name);
  const stripSelf = C.reorderByStrip({
    items: shots,
    fromMediaIndex: beforeCur,
    clientX: C.STRIP_PAD_X + 4 * cell + C.STRIP_THUMB_W * 0.75,
    stripLeft: 0,
    scrollLeft: 0,
    selected: []
  });
  assert.equal(stripSelf.moved, true);
  const stripSelfCur = C.playbackIndexAfterReorder(shots, beforeCur, stripSelf.items);
  assert.equal(C.playbackMedia(stripSelf.items)[stripSelfCur].name, shots[beforeCur].name);
});

test('the slide on screen stays the same after a move', () => {
  const shots = whatsappShots(8);
  const rows = C.stackMediaRows(shots.length);
  const beforeCur = 2;
  const moved = C.reorderByHandle({
    items: shots,
    rows,
    fromItemsIdx: 0,
    clientY: rows[2].top + rows[2].height * 0.75,
    selected: []
  });
  const next = C.playbackIndexAfterReorder(shots, beforeCur, moved.items);
  assert.equal(C.playbackMedia(moved.items)[next].name, shots[2].name);
  assert.notEqual(C.playbackMedia(moved.items)[beforeCur].name, shots[2].name);
  const apply = pageHtml.slice(pageHtml.indexOf('function applyReorderResult'), pageHtml.indexOf('function plHitTest'));
  assert.match(apply, /playbackIndexAfterReorder\(beforeItems, beforeCur, items\)/);
  assert.equal(apply.includes('rebuildSlides(cur)'), false);
  const assignAt = apply.indexOf('cur = nextCur');
  const rebuildAt = apply.indexOf('rebuildSlides(nextCur)');
  assert.ok(assignAt >= 0 && rebuildAt > assignAt);
});

test('clicking a strip thumbnail jumps to that slide', () => {
  const thumb = pageHtml.slice(pageHtml.indexOf('function createStripThumb'), pageHtml.indexOf('function unmountStripThumb'));
  assert.equal(thumb.includes('ti.onclick'), false);
  assert.match(thumb, /tw\.addEventListener\('click', \(\) => goTo\(i\)\)/);
  const start = pageHtml.slice(pageHtml.indexOf('function startPointerReorder'), pageHtml.indexOf('function onReorderPointerMove'));
  assert.equal(start.includes('setPointerCapture'), false);
  const move = pageHtml.slice(pageHtml.indexOf('function onReorderPointerMove'), pageHtml.indexOf('function onReorderPointerEnd'));
  const thresholdAt = move.indexOf('pointerPassedReorderThreshold');
  const captureAt = move.indexOf('setPointerCapture');
  assert.ok(thresholdAt >= 0 && captureAt > thresholdAt);
  const row = pageHtml.slice(pageHtml.indexOf('function createPlMediaRow'), pageHtml.indexOf('function plRowMatchesEl'));
  assert.match(row, /goTo\(thisMI\)/);
  assert.equal(row.includes('setPointerCapture'), false);
  assert.match(row, /div\.onclick = e =>/);
  assert.match(move, /maybeAutoScrollList\(e\.clientY\)/);
  assert.equal(C.pointerPassedReorderThreshold(0, 0), false);
  assert.equal(C.pointerPassedReorderThreshold(3, 0), false);
  assert.equal(C.pointerPassedReorderThreshold(4, 0), true);
});

test('new order holds after Next/Prev and after save and reopen', () => {
  const shots = whatsappShots(8);
  const rows = C.stackMediaRows(shots.length);
  const moved = C.reorderByHandle({
    items: shots,
    rows,
    fromItemsIdx: 0,
    clientY: rows[2].top + rows[2].height * 0.75,
    selected: []
  });
  const cur = C.playbackIndexAfterReorder(shots, 0, moved.items);
  assert.equal(C.playbackMedia(moved.items)[cur].name, shots[0].name);
  const next = C.stepMediaIndex(C.playbackMedia(moved.items).length, cur, false);
  const prev = C.stepMediaIndex(C.playbackMedia(moved.items).length, next, true);
  assert.equal(C.playbackMedia(moved.items)[next].name, shots[3].name);
  assert.equal(prev, cur);
  assert.deepEqual(Array.from(shotNames(moved.items)), [
    shots[1].name, shots[2].name, shots[0].name,
    shots[3].name, shots[4].name, shots[5].name, shots[6].name, shots[7].name
  ]);
  assert.equal(pageHtml.includes('items = result.items'), true);
  assert.equal(pageHtml.includes('schedAutoSave()'), true);
  const doc = C.buildPlaylistDocument({ items: moved.items, savedAt: 1 });
  const opened = C.planSessionRestore(C.normalizePlaylist(JSON.parse(JSON.stringify(doc))).items, () => true);
  assert.deepEqual(Array.from(opened.items.map(it => it.path)), moved.items.map(it => it.path));
  assert.deepEqual(Array.from(opened.items.map(it => it.name)), Array.from(shotNames(moved.items)));
  assert.equal(opened.missing, 0);
});

test('playback follows the new order', () => {
  const shots = whatsappShots(8);
  const moved = C.reorderByStrip({
    items: shots,
    fromMediaIndex: 7,
    clientX: C.STRIP_PAD_X + C.STRIP_THUMB_W * 0.25,
    stripLeft: 0,
    scrollLeft: 0
  });
  const order = [
    shots[7].name, shots[0].name, shots[1].name, shots[2].name,
    shots[3].name, shots[4].name, shots[5].name, shots[6].name
  ];
  assert.deepEqual(Array.from(C.playbackNames(moved.items, 0, 8, false)), order);
  const len = C.playbackMedia(moved.items).length;
  assert.equal(C.playbackNames(moved.items, C.stepMediaIndex(len, 7, false), 1, false)[0], order[0]);
  assert.equal(C.playbackNames(moved.items, 0, 8, true)[1], order[7]);
});

test('dropping files from Explorer still works', () => {
  assert.equal(tauriConf.app.windows[0].dragDropEnabled, true);
  assert.equal(pageHtml.includes('tauri://drag-drop'), true);
  assert.equal(pageHtml.includes('planExplorerFileDrop'), true);
  assert.equal(pageHtml.includes('openMediaPaths(media'), true);
  const shots = whatsappShots(8).map(it => it.path);
  const dropped = shots.concat(['C:\\Users\\John\\Desktop\\notes.txt']);
  const ontoShow = C.planExplorerFileDrop(dropped, { viewerOpen: true, itemCount: 8 });
  assert.deepEqual(Array.from(ontoShow.media), shots);
  assert.deepEqual(Array.from(ontoShow.skipped), ['C:\\Users\\John\\Desktop\\notes.txt']);
  assert.equal(ontoShow.append, true);
  const ontoEmpty = C.planExplorerFileDrop(dropped, { viewerOpen: false, itemCount: 0 });
  assert.equal(ontoEmpty.append, false);
  assert.equal(ontoEmpty.media.length, 8);
  const empty = C.planExplorerFileDrop([], { viewerOpen: true, itemCount: 3 });
  assert.equal(empty.media.length, 0);
  assert.equal(empty.append, true);
});

test('user media is never permanently deleted and the confirm names files', () => {
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  const demo = fs.readFileSync(new URL('./demo/SlideShowPro.html', import.meta.url), 'utf8');
  const core = fs.readFileSync(new URL('./slidex-core.js', import.meta.url), 'utf8');
  const trash = fs.readFileSync(new URL('./src-tauri/src/media_trash.rs', import.meta.url), 'utf8');
  const lib = fs.readFileSync(new URL('./src-tauri/src/lib.rs', import.meta.url), 'utf8');
  [html, demo, core].forEach((text) => {
    assert.doesNotMatch(text, /fileHandle\.remove\s*\(/);
    assert.doesNotMatch(text, /confirmDeleteFile/);
    assert.doesNotMatch(text, /\bunlink(?:Sync)?\s*\(/);
    assert.doesNotMatch(text, /\brm(?:Sync)?\s*\(/);
  });
  const trashImpl = trash.split('#[cfg(test)]')[0];
  assert.match(trashImpl, /trash::delete/);
  assert.doesNotMatch(trashImpl, /remove_file|remove_dir|std::fs::remove/);
  const cmd = lib.slice(lib.indexOf('fn trash_media_files'), lib.indexOf('fn cast_set_playlist'));
  assert.match(cmd, /media_trash::trash_user_media/);
  assert.doesNotMatch(cmd, /remove_file|remove_dir/);
  assert.match(html, /trash_media_files/);
  assert.match(html, /id="delConfirm"/);
  assert.match(html, /id="ctx-del-sep"/);
  assert.match(html, /Move file to Recycle Bin…/);
  assert.match(html, /Shift\+Del/);
  assert.match(html, /Remove from playlist only/);
  assert.match(html, /g\('delCancel'\)\.focus\(\)/);
  assert.match(html, /SlideXCore\.deleteConfirmKey/);
  assert.match(html, /SlideXCore\.deleteConfirmWarnLine/);
  const menu = html.slice(html.indexOf('id="ctxmenu"'), html.indexOf('id="delConfirm"'));
  assert.ok(menu.indexOf('id="ctx-remove"') < menu.indexOf('id="ctx-del-sep"'));
  assert.ok(menu.indexOf('id="ctx-del-sep"') < menu.indexOf('id="ctx-del"'));
  assert.match(menu, /class="ctxi danger"/);

  const files = [
    { name: 'desert_noon_03.jpg', path: 'C:\\Photos\\desert_noon_03.jpg' },
    { name: 'forest_ridge_04.jpg', path: 'C:\\Photos\\forest_ridge_04.jpg' },
    { name: 'abstract_glow_05.jpg', path: 'D:\\Clips\\abstract_glow_05.jpg' }
  ];
  const model = C.deleteConfirmModel(files, {
    inTauri: true,
    otherPlaylists: ['Beach', 'Weekend']
  });
  assert.equal(model.count, 3);
  assert.equal(model.focus, 'cancel');
  assert.deepEqual(model.names, files.map((f) => f.name));
  assert.equal(model.recycleOk, true);
  assert.deepEqual(Array.from(model.actions), ['remove-only', 'cancel', 'recycle']);
  const shown = C.deleteConfirmText(model);
  files.forEach((f) => assert.match(shown, new RegExp(f.name)));
  assert.match(shown, /Move 3 files/);
  assert.match(shown, /Beach/);
  assert.match(shown, /Weekend/);
  assert.equal(C.deleteConfirmKey(model, 'Escape'), 'cancel');
  assert.equal(C.deleteConfirmKey(model, 'Enter'), 'dialog');
  assert.equal(C.deleteConfirmKey(model, ' '), 'dialog');
  assert.equal(C.deleteConfirmKey(model, 'Tab'), 'dialog');
  assert.equal(C.deleteConfirmKey(model, 'Delete'), 'swallow');
  assert.match(C.deleteConfirmWarnLine(model), /Beach/);
  assert.doesNotMatch(C.deleteConfirmWarnLine(model), /Recycle Bin/);
  const kept = C.applyDeleteChoice(files, [0, 1, 2], 'cancel');
  assert.deepEqual(kept.map((f) => f.name), files.map((f) => f.name));
  const removed = C.applyDeleteChoice(files, [0, 1, 2], 'remove-only');
  assert.equal(removed.length, 0);

  const share = C.deleteConfirmModel(
    [{ name: 'clip.mp4', path: '\\\\nas\\media\\clip.mp4' }],
    { inTauri: true }
  );
  assert.equal(share.recycleOk, false);
  assert.deepEqual(Array.from(share.actions), ['remove-only', 'cancel']);
  assert.match(share.body, /clip\.mp4/);
  assert.match(share.body, /network share/);
  assert.match(share.body, /Nothing was deleted/);
  assert.equal(C.deleteConfirmWarnLine(share), '');
  assert.equal(C.deleteConfirmKey(share, 'Escape'), 'cancel');
  assert.equal(C.isNetworkSharePath('\\\\?\\UNC\\nas\\media\\still.jpg'), true);
  assert.equal(C.isNetworkSharePath('//nas/media/other.png'), true);
  assert.equal(C.isNetworkSharePath('C:\\Photos\\lake.jpg'), false);
  assert.equal(C.isNetworkSharePath('\\\\?\\D:\\Videos\\clip.mp4'), false);
  const extended = C.deleteConfirmModel(
    [{ name: 'still.jpg', path: '\\\\?\\UNC\\nas\\media\\still.jpg' }],
    { inTauri: true }
  );
  assert.equal(extended.recycleOk, false);
  assert.match(C.deleteConfirmText(extended), /still\.jpg/);
  const others = C.otherSavedPlaylists(
    files,
    [
      { name: 'Weekend', items: [{ path: 'C:\\Photos\\desert_noon_03.jpg' }] },
      { name: 'Current', items: [{ path: 'C:\\Photos\\desert_noon_03.jpg' }] },
      { name: 'Empty', items: [{ path: 'C:\\Other\\nope.jpg' }] }
    ],
    'Current'
  );
  assert.deepEqual(Array.from(others), ['Weekend']);
});

test('cast connect shows the three steps, then timeout, cancel, and retry reset it', () => {
  const tv = 'Living Room TV';
  let state = C.beginCastConnect(tv);
  assert.equal(state.phase, 'connecting');
  assert.equal(state.connected, false);
  assert.deepEqual(Array.from(C.castConnectStepTexts(state)), [
    'Found Living Room TV',
    'Opening a secure link to the TV… 0 s',
    'Starting the stream'
  ]);
  state = C.tickCastConnect(state, 6000);
  assert.equal(state.phase, 'connecting');
  assert.match(C.castConnectStepTexts(state)[1], /6 s/);
  state = C.advanceCastConnect(state, 2);
  assert.equal(C.castConnectStepMarks(state)[2], 'current');
  const streaming = C.tickCastConnect(state, 20000);
  assert.equal(streaming.phase, 'connecting');
  assert.equal(streaming.step, 2);
  assert.equal(streaming.timedOut, false);
  state = C.tickCastConnect(C.beginCastConnect(tv), 14999);
  assert.equal(state.phase, 'connecting');
  state = C.tickCastConnect(state, 1);
  assert.equal(state.phase, 'failed');
  assert.equal(state.timedOut, true);
  assert.equal(state.connected, false);
  assert.equal(state.active, false);
  assert.equal(state.canRetry, true);
  assert.equal(state.detailsOpen, false);
  assert.equal(
    state.message,
    "Couldn't connect to Living Room TV. Make sure the TV is on and on the same Wi-Fi. If Windows asked about the firewall, allow SlideX on Private networks."
  );
  assert.match(state.details, /timed out \(15 s\)/);
  const opened = C.toggleCastDetails(state);
  assert.equal(opened.detailsOpen, true);
  assert.match(opened.details, /timed out \(15 s\)/);
  const logged = C.withCastLogPath(opened, 'C:\\Users\\me\\AppData\\Local\\SlideX\\logs\\cast.log');
  assert.match(logged.details, /cast\.log/);
  const again = C.retryCastConnect(logged, tv);
  assert.equal(again.phase, 'connecting');
  assert.equal(again.seconds, 0);
  assert.equal(again.connected, false);
  assert.equal(again.rawError, null);
  assert.equal(again.canRetry, false);

  const cancelled = C.cancelCastConnect(C.tickCastConnect(C.beginCastConnect(tv), 4000));
  assert.equal(cancelled.phase, 'idle');
  assert.equal(cancelled.cancelled, true);
  assert.equal(cancelled.connected, false);
  assert.equal(cancelled.active, false);
  assert.equal(cancelled.canRetry, true);
  assert.equal(cancelled.seconds, 0);
  const retried = C.retryCastConnect(cancelled);
  assert.equal(retried.phase, 'connecting');
  assert.equal(retried.tv, tv);
  assert.equal(retried.elapsedMs, 0);

  const failed = C.failCastConnect(
    C.beginCastConnect(tv),
    'TLS handshake to 192.0.2.10:8009 timed out',
    'C:\\Users\\me\\AppData\\Local\\SlideX\\logs\\cast.log',
    false
  );
  assert.equal(failed.detailsOpen, false);
  assert.match(failed.message, /Private networks/);
  assert.match(C.toggleCastDetails(failed).details, /TLS handshake/);
  assert.match(C.toggleCastDetails(failed).details, /cast\.log/);
  assert.equal(C.retryCastConnect(failed).phase, 'connecting');
  assert.equal(C.CAST_CONNECT_TIMEOUT_MS, 15000);

  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /id="castSteps"/);
  assert.match(html, /id="castDetails"/);
  assert.match(html, /id="castCancel"/);
  assert.match(html, /id="castRetry"/);
  assert.match(html, /id="castEnterAddr"/);
  assert.match(html, /If the TV shows a prompt, accept it\./);
  assert.match(html, /SlideXCore\.beginCastConnect|Core\.beginCastConnect/);
  assert.match(html, /tickCastConnect/);
  assert.match(html, /cancelCastConnect/);
  assert.match(html, /failCastConnect/);
  assert.match(html, /retryCastConnect/);
  assert.match(html, /cast_cancel_connect/);
  const beginUi = html.slice(html.indexOf('function beginUiConnect'), html.indexOf('document.getElementById("castStart")'));
  const openedLink = beginUi.indexOf('advanceCastConnect(connectState, 2)');
  const stoppedClock = beginUi.indexOf('clearInterval(connectTimer)', openedLink);
  assert.ok(openedLink !== -1 && stoppedClock !== -1 && stoppedClock - openedLink < 180);
  assert.doesNotMatch(html, /Connecting…/);
  assert.equal(C.castFailureMessage('Living Room TV'), failed.message);
});

test('the Undock button stays in the code and is not shown in 0.1.18', () => {
  assert.equal(C.UNDOCK_MAIN_WINDOW, false);
  assert.equal(C.undockControlShown(false), false);
  assert.equal(C.undockControlShown(true), true);
  const html = fs.readFileSync(new URL('./SlideShowPro.html', import.meta.url), 'utf8');
  assert.match(html, /id="plundock"/);
  assert.match(html, /const SSP_UNDOCK_ENABLED = false/);
  assert.match(html, /if \(SSP_UNDOCK_ENABLED\) document\.body\.classList\.add\('ssp-undock-enabled'\)/);
  assert.match(html, /body\.ssp-tauri\.ssp-undock-enabled #plundock/);
  assert.doesNotMatch(html, /body\.ssp-tauri #plundock \{/);
  const undockFn = html.slice(html.indexOf('async function undockPlaylistWindow'), html.indexOf('let _plCloseBound'));
  assert.match(undockFn, /if \(!SSP_UNDOCK_ENABLED\) return/);
  assert.match(html, /function undockPlaylistWindow/);
  assert.match(html, /open_playlist_window/);
});
