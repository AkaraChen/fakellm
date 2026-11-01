// Raw client for the CONNECT proxy check: CONNECT through the proxy to fakellm, send a streaming chat request, read until
// the connection ends. Prints one line: "RESULT reset" (ECONNRESET), "RESULT eof" (clean EOF/FIN) or "RESULT timeout".
// A complete stream ends with the chunked terminator "0\r\n\r\n"; "complete" is reported separately.
// Usage: node connect-client.mjs PROXY_PORT FAKELLM_PORT
import net from "node:net";

const [proxyPort, targetPort] = process.argv.slice(2).map(Number);
const sock = net.connect(proxyPort, "127.0.0.1");
let phase = "connect";
let buf = "";
let body = "";
const done = (what) => { console.log(`RESULT ${what}${body.endsWith("0\r\n\r\n") ? " complete" : " incomplete"}`); process.exit(0); };
setTimeout(() => done("timeout"), 15000).unref();
sock.on("connect", () => sock.write(`CONNECT 127.0.0.1:${targetPort} HTTP/1.1\r\nHost: 127.0.0.1:${targetPort}\r\n\r\n`));
sock.on("data", (d) => {
  if (phase === "connect") {
    buf += d.toString("latin1");
    const end = buf.indexOf("\r\n\r\n");
    if (end < 0) return;
    if (!buf.startsWith("HTTP/1.1 200")) { console.log(`RESULT connect-failed ${buf.split("\r\n")[0]}`); process.exit(1); }
    phase = "body";
    body = buf.slice(end + 4);
    const payload = JSON.stringify({ model: "m", stream: true, messages: [{ role: "user", content: "hi" }] });
    sock.write(`POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: ${Buffer.byteLength(payload)}\r\n\r\n${payload}`);
    return;
  }
  body += d.toString("latin1");
});
sock.on("error", (e) => done(e.code === "ECONNRESET" ? "reset" : `error-${e.code}`));
sock.on("end", () => done("eof"));
