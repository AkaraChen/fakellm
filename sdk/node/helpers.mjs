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
