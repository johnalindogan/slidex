import { copyFileSync, existsSync, mkdirSync, readdirSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const REQUIRED_FONTS = [
  'PlusJakartaSans-Variable.woff2',
  'JetBrainsMono-Regular.woff2',
  'OFL.txt',
];

const src = join(root, 'SlideShowPro.html');
const destDir = join(root, 'android', 'app', 'src', 'main', 'assets');
const dest = join(destDir, 'SlideShowPro.html');
const fontsSrc = join(root, 'fonts');
const fontsDest = join(destDir, 'fonts');

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

mkdirSync(destDir, { recursive: true });
copyFileSync(src, dest);
copyFileSync(join(root, 'slidex-core.js'), join(destDir, 'slidex-core.js'));
requireFonts(fontsSrc);
mkdirSync(fontsDest, { recursive: true });
for (const name of REQUIRED_FONTS) {
  copyFileSync(join(fontsSrc, name), join(fontsDest, name));
}
console.log('Synced SlideShowPro.html -> android/app/src/main/assets/SlideShowPro.html');
console.log(`Synced fonts/ -> android assets fonts/ (${REQUIRED_FONTS.length} required files)`);
