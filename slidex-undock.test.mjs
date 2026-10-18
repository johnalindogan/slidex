import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';

const VIEWPORT = { width: 560, height: 820 };

function readHtml(name) {
  return fs.readFileSync(new URL(name, import.meta.url), 'utf8');
}

function divAncestors(html, id) {
  const tagRe = /<\/?div\b[^>]*>/gi;
  const stack = [];
  let found = null;
  let match;
  while ((match = tagRe.exec(html))) {
    const tag = match[0];
    if (tag.startsWith('</')) {
      stack.pop();
      continue;
    }
    const idMatch = /\bid="([^"]+)"/.exec(tag);
    const nodeId = idMatch ? idMatch[1] : null;
    if (nodeId === id) found = stack.map((item) => item).filter(Boolean);
    if (!tag.endsWith('/>')) stack.push(nodeId);
  }
  return found;
}

function playlistHidesViewer(html) {
  return /body\.ssp-playlist-window\s+#viewer\s*,|body\.ssp-playlist-window\s+#dz\s*,[\s\S]*?#viewer\s*,[\s\S]*?display\s*:\s*none\s*!important/.test(html)
    || /body\.ssp-playlist-window[\s\S]{0,400}#viewer[\s\S]{0,500}display\s*:\s*none\s*!important/.test(html);
}

/**
 * Used box of #media-dock after the playlist-window stylesheet.
 * A display:none ancestor (the hidden #viewer) yields a 0×0 box.
 */
function playlistDockBox(html, viewport = VIEWPORT) {
  const ancestors = divAncestors(html, 'media-dock');
  assert.ok(ancestors, '#media-dock is missing');
  const viewerHidden = playlistHidesViewer(html);
  if (ancestors.includes('viewer') && viewerHidden) {
    return {
      display: 'none',
      width: 0,
      height: 0,
      ancestors,
      reason: '#media-dock is inside #viewer, which playlist mode sets to display:none'
    };
  }
  const rule = /body\.ssp-playlist-window\s+#media-dock[\s\S]*?\{([^}]*)\}/.exec(html);
  const body = rule ? rule[1] : '';
  const display = /display\s*:\s*([^;!]+)/.exec(body);
  const usedDisplay = display ? display[1].trim() : 'flex';
  if (usedDisplay === 'none') {
    return { display: 'none', width: 0, height: 0, ancestors, reason: 'playlist rule hides #media-dock' };
  }
  const fills = /inset\s*:\s*0/.test(body) || /height\s*:\s*100vh/.test(body);
  return {
    display: usedDisplay,
    width: fills ? viewport.width : 0,
    height: fills ? viewport.height : 0,
    ancestors,
    reason: fills ? 'playlist window' : 'playlist rule does not give #media-dock a size'
  };
}

function extractFunction(src, name) {
  const asyncAt = src.indexOf('async function ' + name);
  const at = asyncAt >= 0 ? asyncAt : src.indexOf('function ' + name + '(');
  if (at < 0) return '';
  const brace = src.indexOf('{', at);
  let depth = 0;
  for (let i = brace; i < src.length; i++) {
    if (src[i] === '{') depth += 1;
    else if (src[i] === '}') {
      depth -= 1;
      if (depth === 0) return src.slice(at, i + 1);
    }
  }
  return '';
}

function extractRustFn(src, name) {
  const at = src.indexOf('fn ' + name);
  if (at < 0) return '';
  const brace = src.indexOf('{', at);
  let depth = 0;
  for (let i = brace; i < src.length; i++) {
    if (src[i] === '{') depth += 1;
    else if (src[i] === '}') {
      depth -= 1;
      if (depth === 0) return src.slice(at, i + 1);
    }
  }
  return '';
}

function assertDockClosesChild(html) {
  assert.doesNotMatch(
    html,
    /tauri:\/\/close-requested/,
    'a close-requested listener on the playlist window prevents the close'
  );
  const bind = extractFunction(html, 'tryBindPlaylistWindowClosed');
  assert.match(bind, /tauri:\/\/destroyed/, 're-dock must listen for tauri://destroyed');
  assert.match(bind, /token !== _plBindToken/, 'destroyed re-dock is not guarded against a stale window');
  const restore = extractFunction(html, 'restoreDockedMediaManager');
  assert.match(restore, /if \(!playlistUndocked\) return/, 're-dock is not idempotent');
  const destroy = extractFunction(html, 'destroyOwnPlaylistWindow');
  assert.match(destroy, /getCurrentWebviewWindow\s*\(\s*\)/);
  assert.match(destroy, /cur\.label === 'playlist'/);
  assert.match(destroy, /\.destroy\s*\(\s*\)/);
  assert.doesNotMatch(destroy, /\.close\s*\(\s*\)/);
  const dock = extractFunction(html, 'dockPlaylistWindow');
  const playlistBranch = (dock.split('if (SSP_IS_PLAYLIST_WINDOW)')[1] || '').split('// Main:')[0];
  const destroyAt = playlistBranch.search(/destroyOwnPlaylistWindow\s*\(/);
  assert.ok(destroyAt >= 0, 'playlist Dock does not destroy its own window');
  assert.doesNotMatch(playlistBranch.slice(0, destroyAt), /\bawait\b/, 'destroy() is queued behind an awaited push or event');
  assert.doesNotMatch(playlistBranch, /broadcastPlaylistSync\s*\(|emit\(\s*'ssp-pl-dock'/);
  assert.match(playlistBranch, /setTimeout\([\s\S]*?,\s*250\)/, 'JS destroy has no short fallback to close_playlist_window');
  assert.match(playlistBranch, /close_playlist_window/);
  assert.doesNotMatch(playlistBranch, /\.close\s*\(\s*\)/);
  const undock = extractFunction(html, 'undockPlaylistWindow');
  assert.doesNotMatch(undock, /finally\s*\{[^}]*_plUndocking\s*=\s*false/, 'undock releases its lock before the window is ready');
  const readyAt = html.indexOf("listen('ssp-pl-ready'");
  assert.match(html.slice(readyAt, readyAt + 500), /_plUndocking\s*=\s*false/);
  const failedAt = html.indexOf("listen('ssp-pl-open-failed'");
  assert.match(html.slice(failedAt, failedAt + 500), /_plUndocking\s*=\s*false/);
  const mainBranch = dock.split('// Main:')[1] || '';
  assert.match(mainBranch, /close_playlist_window/, 'main Dock path does not destroy the child');
  assert.match(mainBranch, /restoreDockedMediaManager\s*\(/);
  const listenerAt = html.indexOf("listen('ssp-pl-dock'");
  assert.ok(listenerAt > 0, 'main window does not listen for ssp-pl-dock');
  const listener = html.slice(listenerAt, listenerAt + 500);
  assert.match(listener, /restoreDockedMediaManager\s*\(/, 'ssp-pl-dock does not re-dock');
  assert.doesNotMatch(html, /closePlaylistWebview/);
}

function assertRustDestroy(lib) {
  const close = extractRustFn(lib, 'close_playlist_window');
  assert.match(close, /\.destroy\s*\(/);
  assert.doesNotMatch(close, /\.close\s*\(/);
  const afterDestroy = close.slice(close.indexOf('.destroy('));
  assert.match(afterDestroy, /get_webview_window\("playlist"\)/, 'close does not check the label after destroy');
  assert.match(close, /eprintln!/, 'destroy errors are discarded');
  assert.doesNotMatch(close, /let _ =/);
  const open = extractRustFn(lib, 'open_playlist_window');
  const labelAt = open.indexOf('get_webview_window("playlist")');
  const createAt = open.indexOf('WebviewWindowBuilder');
  const lockAt = open.indexOf('gate.opening');
  assert.ok(lockAt >= 0 && lockAt < createAt, 'undock has no lock before create');
  assert.ok(labelAt >= 0 && labelAt < createAt, 'playlist label is not checked before create');
  const skipped = open.slice(lockAt, open.indexOf('gate.opening = true'));
  assert.match(skipped, /ssp-pl-ready/, 'a skipped open does not emit ready');
  const wait = extractRustFn(lib, 'wait_until_playlist_destroyed');
  assert.match(wait, /tokio::time::sleep/);
  assert.match(wait, /Duration::from_millis\(25\)/);
  assert.match(wait, /Duration::from_millis\(500\)/);
  assert.match(wait, /get_webview_window\("playlist"\)\.is_none\(\)/);
  assert.doesNotMatch(wait, /thread::sleep/);
  assert.doesNotMatch(open, /thread::sleep/);
  assert.doesNotMatch(lib, /std::thread::sleep\(Duration::from_millis\(20\)\)/);
}

function assertDestroyPermission() {
  const dir = new URL('./src-tauri/capabilities/', import.meta.url);
  const files = fs.readdirSync(dir).filter((name) => name.endsWith('.json'));
  const grants = [];
  for (const name of files) {
    const cap = JSON.parse(fs.readFileSync(new URL(name, dir), 'utf8'));
    for (const perm of cap.permissions || []) {
      const id = typeof perm === 'string' ? perm : perm && perm.identifier;
      if (id === 'core:window:allow-destroy' || id === 'core:window:allow-close') {
        grants.push({ file: name, id, windows: cap.windows || [] });
      }
    }
  }
  const destroy = grants.filter((grant) => grant.id === 'core:window:allow-destroy');
  assert.equal(destroy.length, 1, 'core:window:allow-destroy must be granted once');
  assert.deepEqual(destroy[0].windows, ['playlist'], 'destroy must be scoped to the playlist window, not every window');
  for (const grant of grants) {
    if (grant.id === 'core:window:allow-close') {
      assert.deepEqual(grant.windows, ['playlist'], 'allow-close must be scoped to the playlist window');
    }
  }
}

function assertViewerClass(html) {
  assert.doesNotMatch(html, /#viewer\[style\*="flex"\]/);
  assert.match(html, /body\.ssp-viewer-open\s+#media-dock\s*\{[^}]*display\s*:\s*flex/);
  const shown = extractFunction(html, 'setViewerShown');
  assert.match(shown, /ssp-viewer-open/);
  assert.match(shown, /classList\.toggle\('ssp-viewer-open'/);
}

function assertPlaylistDockVisible(html, label) {
  const box = playlistDockBox(html);
  assert.notEqual(box.display, 'none', label + ' ' + box.reason);
  assert.ok(box.width > 0 && box.height > 0, label + ' box ' + JSON.stringify(box));
  assert.ok(!box.ancestors.includes('viewer'), label + ' #media-dock is still inside #viewer');
}

test('playlist window shows #media-dock at non-zero size', () => {
  const html = readHtml('./SlideShowPro.html');
  assertPlaylistDockVisible(html, 'SlideShowPro.html');
  const uiPath = new URL('./ui/index.html', import.meta.url);
  assert.ok(fs.existsSync(uiPath), 'ui/index.html is missing; run node scripts/sync-ui.mjs');
  const ui = fs.readFileSync(uiPath, 'utf8');
  assert.equal(ui, html, 'ui/index.html is not a copy of SlideShowPro.html');
  assertPlaylistDockVisible(ui, 'ui/index.html');
});

test('Dock destroys the undocked child and re-docks on destroyed', () => {
  const html = readHtml('./SlideShowPro.html');
  const lib = fs.readFileSync(new URL('./src-tauri/src/lib.rs', import.meta.url), 'utf8');
  assertDockClosesChild(html);
  assertRustDestroy(lib);
  assertDestroyPermission();
  assertViewerClass(html);
  const ui = fs.readFileSync(new URL('./ui/index.html', import.meta.url), 'utf8');
  assertDockClosesChild(ui);
  assertViewerClass(ui);
});
