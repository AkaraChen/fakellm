// fakellm policy shim for txiki.js. UNTESTED: written from the txiki.js documentation, no `tjs` binary was available.
// Protocol: see src/policy/protocol.rs. stdout carries the protocol only; console output goes to stderr.
const encoder = new TextEncoder();
const decoder = new TextDecoder();

const send = (message) => tjs.stdout.write(encoder.encode(JSON.stringify(message) + "\n"));
const log = (...args) => console.error(args.map((a) => (typeof a === "string" ? a : JSON.stringify(a))).join(" "));
console.log = console.info = console.debug = console.error;
const describe = (e) => (e instanceof Error ? e.message : String(e));

let decide;
try {
  const mod = await import(tjs.env.FAKELLM_POLICY_MODULE);
  decide = mod.default;
  if (typeof decide !== "function") {
    throw new Error("the module must export a default function decide(request, context)");
  }
} catch (e) {
  await send({ load_error: describe(e) });
  tjs.exit(0);
}
await send({ ready: true });

async function handle(line) {
  if (!line.trim()) return;
  let message;
  try {
    message = JSON.parse(line);
  } catch (e) {
    console.error(`fakellm shim: unreadable input line: ${describe(e)}`);
    return;
  }
  const { id, request, state, hits } = message;
  try {
    request.tools ??= []; // the IR omits empty collections; policies should not have to
    request.stop ??= [];
    const out = await decide(request, { state, hits, log });
    if (out === null || out === undefined || (typeof out === "object" && !Array.isArray(out) && out.decline === true)) {
      // "not mine": fakellm moves on to the next script rule
      const kept = out ? out.state : undefined;
      await send(kept === undefined ? { id, decline: true } : { id, decline: true, state: kept });
      return;
    }
    if (typeof out !== "object" || Array.isArray(out)) {
      throw new Error("decide must return an object (a ReplySpec, optionally with a `state` field), null or {decline: true}");
    }
    const { state: next, ...reply } = out;
    await send(next === undefined ? { id, reply } : { id, reply, state: next });
  } catch (e) {
    await send({ id, error: describe(e) });
  }
}

const buffer = new Uint8Array(65536);
let pending = "";
for (;;) {
  const n = await tjs.stdin.read(buffer);
  if (!n) break;
  pending += decoder.decode(buffer.subarray(0, n), { stream: true });
  let newline;
  while ((newline = pending.indexOf("\n")) >= 0) {
    const line = pending.slice(0, newline);
    pending = pending.slice(newline + 1);
    await handle(line);
  }
}
tjs.exit(0);
