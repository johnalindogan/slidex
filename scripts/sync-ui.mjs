import { copyFileSync, mkdirSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const src = join(root, 'SlideShowPro.html');
const destDir = join(root, 'ui');
const dest = join(destDir, 'index.html');
const fontsSrc = join(root, 'fonts');
const fontsDest = join(destDir, 'fonts');
mkdirSync(destDir, { recursive: true });
mkdirSync(fontsDest, { recursive: true });
copyFileSync(src, dest);
copyFileSync(join(root, 'slidex-core.js'), join(destDir, 'slidex-core.js'));
for (const name of readdirSync(fontsSrc)) {
  copyFileSync(join(fontsSrc, name), join(fontsDest, name));
}
console.log('Synced SlideShowPro.html -> ui/index.html');
console.log(`Synced fonts/ -> ui/fonts/ (${readdirSync(fontsDest).length} files)`);
