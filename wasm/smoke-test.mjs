// Smoke test for the wasm package: runs the server in-process over two web streams, opens a
// sample model with one broken instance and expects a diagnostic for it plus a hover, then checks
// that `serve` resolves on a closed input and rejects when either stream fails.
// Usage: bun wasm/smoke-test.mjs [pkg-dir]   (the `wasm-pack build --target web` output)

import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const pkgDir = resolve(process.argv[2] ?? "pkg");
const { default: init, serve } = await import(
  pathToFileURL(resolve(pkgDir, "ifc_language_server.js")).href
);
await init({
  module_or_path: await readFile(resolve(pkgDir, "ifc_language_server_bg.wasm")),
});

const toServer = new TransformStream();
const fromServer = new TransformStream();
const served = serve(toServer.readable, fromServer.writable);
const writer = toServer.writable.getWriter();
const encoder = new TextEncoder();

function send(message) {
  const body = encoder.encode(JSON.stringify({ jsonrpc: "2.0", ...message }));
  writer.write(encoder.encode(`Content-Length: ${body.length}\r\n\r\n`));
  writer.write(body);
}

const pending = new Map();
const notifications = [];
let notify = () => {};
let nextId = 1;

function request(method, params) {
  const id = nextId++;
  send({ id, method, params });
  return new Promise((resolve) => pending.set(id, resolve));
}

function dispatch(message) {
  if (message.method && message.id !== undefined) {
    send({ id: message.id, result: null });
  } else if (message.method) {
    notifications.push(message);
    notify();
  } else {
    pending.get(message.id)(message);
    pending.delete(message.id);
  }
}

(async () => {
  const reader = fromServer.readable.getReader();
  const decoder = new TextDecoder();
  let buffer = new Uint8Array();
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    buffer = new Uint8Array([...buffer, ...value]);
    for (;;) {
      const header = decoder.decode(buffer.subarray(0, 64));
      const headerEnd = header.indexOf("\r\n\r\n");
      if (headerEnd < 0) break;
      const length = Number(/Content-Length: (\d+)/.exec(header)[1]);
      const start = headerEnd + 4;
      if (buffer.length < start + length) break;
      dispatch(JSON.parse(decoder.decode(buffer.subarray(start, start + length))));
      buffer = buffer.subarray(start + length);
    }
  }
})();

async function notification(method, matches) {
  for (;;) {
    const found = notifications.find((n) => n.method === method && matches(n.params));
    if (found) return found.params;
    await new Promise((resolve) => (notify = resolve));
  }
}

function check(condition, message) {
  if (!condition) {
    console.error(`FAIL: ${message}`);
    process.exit(1);
  }
  console.log(`ok: ${message}`);
}

/** Settles `promise` within five seconds, as `{ resolved }`, `{ error }` or `{ timedOut }`. */
function settled(promise) {
  return Promise.race([
    promise.then(
      () => ({ resolved: true }),
      (error) => ({ error }),
    ),
    new Promise((resolve) => setTimeout(() => resolve({ timedOut: true }), 5_000)),
  ]);
}

setTimeout(() => {
  console.error("FAIL: timed out");
  process.exit(1);
}, 60_000);

const sample = await readFile(
  new URL("../samples/Building-Architecture-IFC4.ifc", import.meta.url),
  "utf8",
);
const broken = "#99999=IFCWALL('2Ndyd$OSX7s9A04nc4lyyf');";
const text = sample.replace("ENDSEC;\nEND-ISO", `${broken}\nENDSEC;\nEND-ISO`);
const brokenLine = text.split("\n").findIndex((line) => line === broken);
check(brokenLine > 0, "sample has the broken instance");
const uri = "inmemory://model/Building-Architecture-IFC4.ifc";

const initialized = await request("initialize", { processId: null, rootUri: null, capabilities: {} });
check(initialized.result?.capabilities?.hoverProvider, "initialize returns capabilities");
send({ method: "initialized", params: {} });
send({
  method: "textDocument/didOpen",
  params: { textDocument: { uri, languageId: "ifc", version: 1, text } },
});

const { diagnostics } = await notification(
  "textDocument/publishDiagnostics",
  (params) => params.uri === uri && params.diagnostics.length > 0,
);
check(
  diagnostics.some((d) => d.range.start.line === brokenLine),
  `diagnostic on the broken instance (${diagnostics.length} total)`,
);

const hover = await request("textDocument/hover", {
  textDocument: { uri },
  position: { line: brokenLine, character: 9 },
});
check(JSON.stringify(hover.result ?? "").includes("IfcWall"), "hover documents IfcWall");

await request("shutdown", null);
send({ method: "exit" });
await writer.close();
check((await settled(served)).resolved, "serve resolves once input closes");

const outputFailure = new Error("output failed");
const { writable: hostInput, readable: serverInput } = new TransformStream();
const failingOutput = new WritableStream({
  write() {
    throw outputFailure;
  },
});
const hostWriter = hostInput.getWriter();
const failedOutput = settled(serve(serverInput, failingOutput));
const body = encoder.encode(
  JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { capabilities: {} } }),
);
hostWriter.write(encoder.encode(`Content-Length: ${body.length}\r\n\r\n`));
hostWriter.write(body);
check((await failedOutput).error === outputFailure, "serve rejects when writing output fails");

const inputFailure = new Error("input failed");
const erroredInput = new ReadableStream({ start: (controller) => controller.error(inputFailure) });
const failedInput = await settled(serve(erroredInput, new WritableStream()));
check(failedInput.error === inputFailure, "serve rejects when input errors");

process.exit(0);
