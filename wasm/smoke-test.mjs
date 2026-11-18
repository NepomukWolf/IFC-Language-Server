// Smoke test for the wasm package: runs the server in-process over two web streams, opens a
// sample model with one broken instance and expects a diagnostic for it plus a hover, removes it
// in a burst of edits and expects debounced diagnostics without it, then checks
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

// Two edits in a burst: the second waits for the debounce timer, then removes the broken instance.
function change(version, range, newText) {
  send({
    method: "textDocument/didChange",
    params: { textDocument: { uri, version }, contentChanges: [{ range, text: newText }] },
  });
}
change(2, { start: { line: brokenLine, character: 8 }, end: { line: brokenLine, character: 8 } }, " ");
change(3, { start: { line: brokenLine, character: 0 }, end: { line: brokenLine + 1, character: 0 } }, "");
const debounced = await notification(
  "textDocument/publishDiagnostics",
  (params) => params.uri === uri && !params.diagnostics.some((d) => d.range.start.line === brokenLine),
);
check(
  !debounced.diagnostics.some((d) => d.range.start.line === brokenLine),
  "debounced diagnostics follow the last edit of a burst",
);

const smallUri = "inmemory://model/small.ifc";
const smallText = "ISO-10303-21;HEADER;FILE_SCHEMA(('IFC4'));ENDSEC;DATA;\n#1=IFCWALL('g',$,'é😀',$);\n#2=IFCX(#1);\nENDSEC;END-ISO-10303-21;";
send({
  method: "textDocument/didOpen",
  params: { textDocument: { uri: smallUri, languageId: "ifc", version: 1, text: smallText } },
});
const counts = await request("ifc/entityCounts", { uri: smallUri });
check(counts.result?.total === 2 && counts.result.entities[0].entity === "IFCWALL",
  "entityCounts reports both instances in count/name order");
const found = await request("ifc/findInstances", { uri: smallUri, query: "é😀", limit: 1 });
check(found.result?.total === 1 && found.result.instances[0].id === 1,
  "findInstances matches a decoded Unicode name");
const prepared = await request("textDocument/prepareCallHierarchy", {
  textDocument: { uri: smallUri }, position: { line: 1, character: 1 },
});
check(prepared.result?.[0]?.data?.id === 1, "call hierarchy prepares an instance");
const incoming = await request("callHierarchy/incomingCalls", { item: prepared.result[0] });
check(incoming.result?.length === 1 && incoming.result[0].from.data.id === 2,
  "call hierarchy finds the referring instance");
const outgoing = await request("callHierarchy/outgoingCalls", { item: incoming.result[0].from });
check(outgoing.result?.length === 1 && outgoing.result[0].to.data.id === 1,
  "call hierarchy resolves the outgoing reference");
await request("ifc/instances", { uri, limit: 0 });
const listed = await request("ifc/instances", { uri: smallUri, limit: 1 });
check(listed.result?.total === 2 && listed.result.instances.length === 1 &&
  listed.result.instances[0].attributes[2].value.value === "é😀" &&
  listed.result.instances[0].range.end.character === 29,
  "instances reloads the statement index and preserves Unicode values and byte ranges");
send({
  method: "textDocument/didChange",
  params: { textDocument: { uri: smallUri, version: 2 }, contentChanges: [{
    range: { start: { line: 2, character: 9 }, end: { line: 2, character: 10 } }, text: "9",
  }] },
});
const edited = await request("ifc/instances", { uri: smallUri });
check(edited.result?.instances[1].attributes[0].value.id === 9,
  "instances reads incremental edits");
const afterEdit = await request("callHierarchy/incomingCalls", { item: prepared.result[0] });
check(afterEdit.result?.length === 0, "call hierarchy reads the edited text index");
const unopened = await request("ifc/instances", { uri: "inmemory://model/unopened.ifc" });
check(unopened.result === null, "instances returns null for unopened documents");

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
