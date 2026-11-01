// fakellm policy shim for Node (also used by Bun). Protocol: see src/policy/protocol.rs.
// stdout carries the protocol only; console.log and friends go to stderr, which fakellm keeps as the policy's log.
import { createInterface } from "node:readline";
import { writeSync } from "node:fs";
import { pathToFileURL } from "node:url";

const send = (message) => writeSync(1, JSON.stringify(message) + "\n");
const log = (...args) => console.error(args.map((a) => (typeof a === "string" ? a : JSON.stringify(a))).join(" "));
console.log = console.info = console.debug = console.error;
const describe = (e) => (e instanceof Error ? e.message : String(e));

let decide;
try {
  const mod = await import(pathToFileURL(process.env.FAKELLM_POLICY_MODULE).href);
  decide = mod.default;
  if (typeof decide !== "function") {
    throw new Error("the module must export a default function decide(request, context)");
  }
} catch (e) {
  send({ load_error: describe(e) });
  process.exit(0);
}
send({ ready: true });

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
      send(kept === undefined ? { id, decline: true } : { id, decline: true, state: kept });
      return;
    }
    if (typeof out !== "object" || Array.isArray(out)) {
      throw new Error("decide must return an object (a ReplySpec, optionally with a `state` field), null or {decline: true}");
    }
    const { state: next, ...reply } = out;
    send(next === undefined ? { id, reply } : { id, reply, state: next });
  } catch (e) {
    send({ id, error: describe(e) });
  }
}

let queue = Promise.resolve();
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on("line", (line) => {
  queue = queue.then(() => handle(line));
});
lines.on("close", () => process.exit(0));
