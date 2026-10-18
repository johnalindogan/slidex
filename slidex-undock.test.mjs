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

function assertDockClosesChild(html, lib) {
  const closer = extractFunction(html, 'closePlaylistWebview');
  assert.ok(closer, 'Dock has no closePlaylistWebview(); the child stays open after re-dock');
  assert.match(closer, /getCurrentWebviewWindow\s*\(\s*\)/);
  assert.match(closer, /\.close\s*\(\s*\)/);
  const dock = extractFunction(html, 'dockPlaylistWindow');
  const playlistBranch = dock.split('if (SSP_IS_PLAYLIST_WINDOW)')[1] || '';
  const mainBranch = playlistBranch.split('// Main:')[1] ? '' : playlistBranch;
  assert.match(playlistBranch, /closePlaylistWebview\s*\(/, 'playlist Dock does not close the child webview');
  assert.match(dock, /closePlaylistWebview\s*\(/);
  assert.ok(mainBranch || dock.includes('closePlaylistWebview'), 'main Dock path must close the child too');
  const listenerAt = html.indexOf("listen('ssp-pl-dock'");
  assert.ok(listenerAt > 0, 'main window does not listen for ssp-pl-dock');
  const listener = html.slice(listenerAt, listenerAt + 900);
  assert.match(listener, /closePlaylistWebview\s*\(/, 'ssp-pl-dock restores the panel without closing the child');
  assert.match(lib, /async fn close_playlist_window/, 'close_playlist_window is a sync command and deadlocks WebView2 when the child closes itself');
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

test('Dock closes the undocked child window', () => {
  const html = readHtml('./SlideShowPro.html');
  const lib = fs.readFileSync(new URL('./src-tauri/src/lib.rs', import.meta.url), 'utf8');
  assertDockClosesChild(html, lib);
  const ui = fs.readFileSync(new URL('./ui/index.html', import.meta.url), 'utf8');
  assertDockClosesChild(ui, lib);
});
