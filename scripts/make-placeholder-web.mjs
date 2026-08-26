#!/usr/bin/env node
// Writes a minimal placeholder bundle into www/ so the Tauri shell can compile
// and run WITHOUT building the full openframe-frontend export (the tauri
// generate_context! macro embeds www/ at compile time, so the directory must
// exist). Swap in the real bundle with `npm run build:web`.
//
// --if-missing: exit quietly when www/index.html already exists (used by the
// predev hook so it never clobbers a real staged bundle).
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const www = join(root, 'www');
const index = join(www, 'index.html');

if (process.argv.includes('--if-missing') && existsSync(index)) {
  process.exit(0);
}

mkdirSync(www, { recursive: true });

const html = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>OpenFrame Console — dev shell</title>
    <style>
      :root { color-scheme: dark; }
      body { margin: 0; min-height: 100vh; display: grid; place-items: center;
             font: 16px/1.5 -apple-system, system-ui, sans-serif;
             background: #161616; color: #f5f5f5; padding: 24px; }
      .card { max-width: 30rem; text-align: center; }
      h1 { font-size: 1.5rem; margin: 0 0 .5rem; }
      code { background: #262626; padding: .1rem .4rem; border-radius: .3rem; }
      .ok { color: #4ade80; }
      pre { text-align: left; background: #0d0d0d; padding: 12px; border-radius: 8px; overflow: auto; }
    </style>
  </head>
  <body>
    <div class="card">
      <h1>OpenFrame Console</h1>
      <p class="ok">✓ Tauri shell is running.</p>
      <p>This is the placeholder bundle. Run <code>npm run build:web</code> to stage the
         real openframe-frontend export instead.</p>
      <pre id="env">window.__ENV = (not injected)</pre>
    </div>
    <script>
      document.getElementById('env').textContent =
        'window.__ENV = ' + JSON.stringify(window.__ENV ?? null, null, 2);
    </script>
  </body>
</html>
`;

writeFileSync(index, html);
console.log('▸ wrote placeholder www/index.html');
