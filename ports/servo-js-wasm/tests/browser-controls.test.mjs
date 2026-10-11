import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { createServoWorkerRuntime } from '../worker-adapter.mjs';

const wasmPath = process.env.SERVO_WASM_PATH ??
  new URL('../../../target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm', import.meta.url);
const wasm = new WebAssembly.Module(readFileSync(wasmPath));
const html = '<!doctype html><html><head><title>browser controls</title></head>' +
  `<body><input id="name"><button id="run" onclick="document.getElementById('result').textContent=document.getElementById('name').value">Run</button>` +
  '<p id="result">waiting</p></body></html>';
const cleanup = (runtime) => { try { runtime.reset(); } catch {} };

// Reproduce the load -> cooperative pump -> evaluate sequence used by MCP.
test('MCP inline navigation sequence exposes the supplied document', { timeout: 20000 }, async () => {
  const runtime = await createServoWorkerRuntime(wasm, {
    width: 800, height: 600,
    fetchImpl: async (url) => { throw new Error(`Unexpected external fetch: ${url}`); },
  });
  try {
    assert.equal(runtime.loadHtml(html), true);
    await runtime.pumpCooperatively({ maxTurns: 2000 });
    const result = await runtime.evaluate('document.title', { maxDurationMs: 2000 });
    assert.deepEqual(result, { Ok: { String: 'browser controls' } });
    const png = await runtime.screenshot({ maxDurationMs: 3000, maxPasses: 8 });
    assert.deepEqual([...png.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10]);
  } finally { cleanup(runtime); }
});

test('MCP navigation sequence does not report a delayed document as ready', { timeout: 20000 }, async () => {
  const runtime = await createServoWorkerRuntime(wasm, {
    fetchImpl: async () => {
      await new Promise((resolve) => setTimeout(resolve, 50));
      return new Response(html, { headers: { 'content-type': 'text/html; charset=utf-8' } });
    },
  });
  try {
    assert.equal(runtime.loadPage('https://document.example/'), true);
    await runtime.pumpCooperatively({ maxTurns: 2000 });
    const result = await runtime.evaluate('document.title', { maxDurationMs: 2000 });
    assert.deepEqual(result, { Ok: { String: 'browser controls' } });
  } finally { cleanup(runtime); }
});

test('rejecting a concurrent screenshot does not request another frame', { timeout: 20000 }, async () => {
  const runtime = await createServoWorkerRuntime(wasm, {
    width: 800, height: 600,
    fetchImpl: async () => new Response(html, { headers: { 'content-type': 'text/html; charset=utf-8' } }),
  });
  let reader;
  try {
    assert.equal(runtime.loadHtml(html), true);
    assert.equal((await runtime.pumpUntilSettled({ maxDurationMs: 5000 })).settled, true);
    const stream = await runtime.screenshotStream({ waitForResources: true, maxDurationMs: 5000, maxPasses: 8 });
    reader = stream.getReader();
    const originalRequestFrame = runtime.instance.exports.servo_worker_request_frame;
    let requests = 0;
    runtime.instance.exports.servo_worker_request_frame = (...args) => {
      requests++;
      return originalRequestFrame(...args);
    };
    try {
      await assert.rejects(runtime.screenshotStream({ maxPasses: 8 }), /already active/);
      assert.equal(requests, 0, 'the rejected capture must leave the active capture untouched');
    } finally {
      runtime.instance.exports.servo_worker_request_frame = originalRequestFrame;
    }
  } finally {
    if (reader) await reader.cancel();
    cleanup(runtime);
  }
});

test('a screenshot resource-wait timeout is not treated as a settled frame', { timeout: 20000 }, async () => {
  const runtime = await createServoWorkerRuntime(wasm, {
    fetchImpl: async () => new Response(html, { headers: { 'content-type': 'text/html; charset=utf-8' } }),
  });
  let stream;
  const originalPump = runtime.pumpUntilSettled;
  try {
    assert.equal(runtime.loadHtml(html), true);
    assert.equal((await runtime.pumpUntilSettled({ maxDurationMs: 5000 })).settled, true);
    runtime.pumpUntilSettled = async () => ({ settled: false, turns: 1, scriptsTerminated: 0 });
    await assert.rejects(async () => {
      stream = await runtime.screenshotStream({ waitForResources: true, maxDurationMs: 5000, maxPasses: 1 });
    }, /budget|settle/i);
  } finally {
    runtime.pumpUntilSettled = originalPump;
    if (stream) await stream.cancel();
    cleanup(runtime);
  }
});
