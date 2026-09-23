import { readFileSync } from 'node:fs';
import { createServoWorkerRuntime } from '../worker-adapter.mjs';

// Diagnostic only: Node process CPU is not Cloudflare billing CPU. Keeping
// the fixture tiny makes this useful for tracking whether the port is even
// close to the Workers Free per-request budget.
const wasmPath = process.env.SERVO_WASM_PATH ??
  new URL('../../../target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm', import.meta.url);
const wasm = new WebAssembly.Module(readFileSync(wasmPath));
const fixture = '<!doctype html><html><head><title>CPU fixture</title></head>' +
  '<body><main id="answer">ok</main></body></html>';

function cpuMs(since) {
  const usage = process.cpuUsage(since);
  return (usage.user + usage.system) / 1000;
}

const start = process.cpuUsage();
const runtime = await createServoWorkerRuntime(wasm, {
  fetchImpl: async () => new Response(fixture, {
    headers: { 'content-type': 'text/html; charset=utf-8' },
  }),
});
const bootstrapCpuMs = cpuMs(start);

for (let i = 0; i < 3; i++) {
  runtime.pump();
  await new Promise((resolve) => setTimeout(resolve, 0));
}

const pageStart = process.cpuUsage();
const pumpCpuMs = [];
runtime.loadPage('https://example.test/');
for (let i = 0; i < 60; i++) {
  const pumpStart = process.cpuUsage();
  runtime.pump();
  pumpCpuMs.push(cpuMs(pumpStart));
  await new Promise((resolve) => setTimeout(resolve, 0));
}
runtime.evaluatePage('document.querySelector("#answer")?.textContent === "ok" ? 42 : 0');
for (let i = 0; i < 10; i++) {
  runtime.pump();
  await new Promise((resolve) => setTimeout(resolve, 0));
}
const pageCpuMs = cpuMs(pageStart);
const result = runtime.pageResult();
if (result?.Ok?.Number !== 42) {
  throw new Error(`CPU fixture did not finish: ${JSON.stringify(result)}`);
}

console.log(JSON.stringify({
  bootstrapCpuMs,
  pageCpuMs,
  maxPumpCpuMs: Math.max(...pumpCpuMs),
  pumpsOver10Ms: pumpCpuMs.filter((cpu) => cpu > 10).length,
  note: 'Node diagnostic; verify account limits in a real Cloudflare environment',
}, null, 2));
