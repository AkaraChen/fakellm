import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { start } from '../js/dist/index.js';

const here = path.dirname(fileURLToPath(import.meta.url));

// One implementation: the sdk/js package (must be built: `npm run build` in sdk/js).
export async function startServer() {
  const bin = process.env.FAKELLM_BIN ?? path.resolve(here, '../../target/debug/fakellm');
  const srv = await start({ bin });
  return {
    baseURL: srv.baseURL,
    url: srv.url,
    ctl: {
      put: (p, body, headers) => srv.ctl('PUT', p, body, headers),
      get: (p) => srv.ctl('GET', p),
      post: (p, body) => srv.ctl('POST', p, body),
      delete: (p) => srv.ctl('DELETE', p),
    },
    stop: () => srv.stop(),
  };
}

// The recorded Chat Completions stream in docs/research.md (ai-sdk fixture): its `data:` lines, one per event.
export function recordedChatEvents() {
  const doc = readFileSync(path.resolve(here, '../../docs/research.md'), 'utf8').split('\n');
  const start = doc.findIndex((l) => l.startsWith('## 真实 OpenAI 流式样本'));
  const first = doc.findIndex((l, i) => i > start && l.startsWith('data: '));
  const events = [];
  for (let i = first; doc[i]?.startsWith('data: '); i++) events.push(doc[i]);
  return events;
}
