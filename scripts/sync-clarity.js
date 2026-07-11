// Vendor Microsoft Clarity's runtime into the renderer.
//
// Tolerant by design: the analytics package is optional at dev time, and a
// missing node_modules copy must not break `tauri dev`. If the source is gone
// but a previously vendored copy already exists, keep it and just warn. Only
// when there is neither a source nor a vendored copy do we skip with a notice.
const fs = require('fs');
const path = require('path');

const src = path.join('node_modules', '@microsoft', 'clarity');
const dst = path.join('src', 'renderer', 'vendor', 'clarity');

fs.mkdirSync(path.join(dst, 'src'), { recursive: true });

try {
  fs.copyFileSync(path.join(src, 'index.js'), path.join(dst, 'index.js'));
  fs.copyFileSync(path.join(src, 'src', 'utils.js'), path.join(dst, 'src', 'utils.js'));
} catch (e) {
  if (fs.existsSync(path.join(dst, 'index.js'))) {
    console.warn('[sync:clarity] @microsoft/clarity not installed; kept existing vendored copy');
  } else {
    console.warn('[sync:clarity] @microsoft/clarity not installed and no vendored copy; skipping (' + e.message + ')');
  }
}
