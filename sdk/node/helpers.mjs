import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));

export async function startServer() {
  const bin = process.env.FAKELLM_BIN ?? path.resolve(here, '../../target/debug/fakellm');
  const child = spawn(bin, ['--port', '0'], { stdio: ['ignore', 'pipe', 'inherit'] });
  const url = await new Promise((resolve, reject) => {
    const rl = createInterface({ input: child.stdout });
    rl.on('line', (line) => {
      if (/^http:\/\/\S+$/.test(line.trim())) resolve(line.trim());
    });
    child.on('error', reject);
    child.on('exit', (code) => reject(new Error(`fakellm exited early (${code})`)));
  });

  const call = async (method, p, body, headers = {}) => {
    const res = await fetch(`${url}/_fakellm${p}`, {
      method,
      headers: { 'content-type': 'application/json', ...headers },
      body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
    });
    const text = await res.text();
    if (!res.ok) throw new Error(`${method} ${p} -> ${res.status}: ${text}`);
    return text ? JSON.parse(text) : null;
  };

  return {
    baseURL: `${url}/v1`,
    url,
    ctl: {
      put: (p, body, headers) => call('PUT', p, body, headers),
      get: (p) => call('GET', p),
      post: (p, body) => call('POST', p, body),
      delete: (p) => call('DELETE', p),
    },
    async stop() {
      child.removeAllListeners('exit');
      child.kill();
      await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
    },
  };
}
