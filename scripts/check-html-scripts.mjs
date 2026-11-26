import { copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const androidDir = join(root, 'android', 'app', 'src', 'main', 'assets');
const androidHtml = join(androidDir, 'SlideShowPro.html');
const fontsSrc = join(root, 'fonts');
const REQUIRED_FONTS = [
  'PlusJakartaSans-Variable.woff2',
  'JetBrainsMono-Regular.woff2',
  'OFL.txt',
];

function requireFonts(dir) {
  if (!existsSync(dir)) {
    console.error(`fonts/: required directory missing: ${dir}`);
    process.exit(1);
  }
  let names;
  try {
    names = new Set(readdirSync(dir));
  } catch (err) {
    console.error(`fonts/: cannot read ${dir}: ${err.message}`);
    process.exit(1);
  }
  const missing = REQUIRED_FONTS.filter((name) => !names.has(name));
  if (missing.length) {
    console.error(`fonts/: missing required file(s): ${missing.join(', ')}`);
    process.exit(1);
  }
  for (const name of REQUIRED_FONTS) {
    const path = join(dir, name);
    let st;
    try {
      st = statSync(path);
    } catch (err) {
      console.error(`fonts/: cannot stat required file ${name}: ${err.message}`);
      process.exit(1);
    }
    if (!st.isFile() || st.size <= 0) {
      console.error(`fonts/: required file is empty or not a file: ${name}`);
      process.exit(1);
    }
  }
}

const core = join(root, 'slidex-core.js');
requireFonts(fontsSrc);
mkdirSync(androidDir, { recursive: true });
mkdirSync(join(root, 'ui'), { recursive: true });
mkdirSync(join(root, 'ui', 'fonts'), { recursive: true });
mkdirSync(join(androidDir, 'fonts'), { recursive: true });
copyFileSync(join(root, 'SlideShowPro.html'), androidHtml);
copyFileSync(core, join(root, 'ui', 'slidex-core.js'));
copyFileSync(core, join(androidDir, 'slidex-core.js'));
for (const name of REQUIRED_FONTS) {
  copyFileSync(join(fontsSrc, name), join(root, 'ui', 'fonts', name));
  copyFileSync(join(fontsSrc, name), join(androidDir, 'fonts', name));
}

const targets = [
  join(root, 'SlideShowPro.html'),
  join(root, 'demo', 'SlideShowPro.html'),
  androidHtml,
];

function checkHtml(htmlPath) {
  const html = readFileSync(htmlPath, 'utf8');
  const re = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
  const dir = mkdtempSync(join(tmpdir(), 'ssp-scripts-'));
  let n = 0;
  let failed = false;
  const label = htmlPath.startsWith(root) ? htmlPath.slice(root.length + 1) : htmlPath;
  try {
    for (const match of html.matchAll(re)) {
      const attrs = match[1] || '';
      const body = match[2] || '';
      if (/\bsrc\s*=/i.test(attrs)) continue;
      if (!body.trim()) continue;
      n += 1;
      const file = join(dir, `inline-${n}.js`);
      writeFileSync(file, body);
      const result = spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
      if (result.status !== 0) {
        failed = true;
        const detail = `${result.stderr || ''}${result.stdout || ''}`.trim();
        console.error(`${label} inline script #${n} failed node --check`);
        if (detail) console.error(detail);
      }
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  if (n === 0) {
    console.error(`No inline scripts found in ${label}`);
    return false;
  }
  if (failed) return false;
  console.log(`Checked ${n} inline script(s) in ${label}`);
  return true;
}

const bareConfirm = /(?<![\w$.])confirm\s*\(/;
const confirmSources = [
  join(root, 'SlideShowPro.html'),
  join(root, 'demo', 'SlideShowPro.html'),
  core,
  androidHtml,
];

let ok = true;
for (const path of confirmSources) {
  const text = readFileSync(path, 'utf8');
  if (bareConfirm.test(text)) {
    ok = false;
    const label = path.startsWith(root) ? path.slice(root.length + 1) : path;
    console.error(`${label} has a bare confirm() call`);
  }
}

for (const path of targets) ok = checkHtml(path) && ok;

const coreCheck = spawnSync(process.execPath, ['--check', core], { encoding: 'utf8' });
if (coreCheck.status !== 0) {
  ok = false;
  console.error('slidex-core.js failed node --check');
  const detail = `${coreCheck.stderr || ''}${coreCheck.stdout || ''}`.trim();
  if (detail) console.error(detail);
} else {
  console.log('Checked slidex-core.js');
}

if (!ok) process.exit(1);
