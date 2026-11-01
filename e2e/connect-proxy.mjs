// A tiny HTTP CONNECT proxy that pipes sockets, for checking what a client sees when fakellm resets the connection behind
// a proxy. Usage: node connect-proxy.mjs [naive|rst]; prints "listening PORT" on stdout.
//   naive: socket.pipe(), and when the upstream side errors (ECONNRESET) destroy the client side normally. This is what a
//          typical node/pipe-based proxy does; the client's TCP stack sees a FIN.
//   rst:   same, but when the upstream side errors, reset the client side too (resetAndDestroy), forwarding the RST.
import net from "node:net";

const mode = process.argv[2] ?? "naive";
const server = net.createServer((client) => {
  let head = Buffer.alloc(0);
  const onData = (chunk) => {
    head = Buffer.concat([head, chunk]);
    const end = head.indexOf("\r\n\r\n");
    if (end < 0) return;
    client.off("data", onData);
    const m = /^CONNECT ([^ :]+):(\d+) HTTP\/1\.[01]/.exec(head.toString("latin1"));
    if (!m) { client.end("HTTP/1.1 400 Bad Request\r\n\r\n"); return; }
    const rest = head.subarray(end + 4);
    const upstream = net.connect(Number(m[2]), m[1], () => {
      client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (rest.length) upstream.write(rest);
      client.pipe(upstream);
      upstream.pipe(client);
    });
    upstream.on("error", (err) => {
      console.log(`upstream error: ${err.code}`);
      if (mode === "rst") client.resetAndDestroy(); else client.destroy();
    });
    client.on("error", () => upstream.destroy());
    client.on("close", () => upstream.destroy());
  };
  client.on("data", onData);
  client.on("error", () => {});
});
server.listen(0, "127.0.0.1", () => console.log(`listening ${server.address().port}`));
