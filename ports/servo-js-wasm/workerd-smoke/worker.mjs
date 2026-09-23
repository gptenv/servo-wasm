import servoWasm from '../../../target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm';
import { createServoWorkerRuntime } from '../worker-adapter.mjs';
import { webPlatformCases } from '../tests/web-platform-cases.mjs';

const FIXTURE_URL = 'https://example.test/';
const FIXTURE_HTML = '<!doctype html><html><head><title>workerd fixture</title>' +
  '<style>#result { color: green }</style></head><body>' +
  '<main id="result">served in workerd</main><script>' +
  'fetch("fixture.json").then(r => r.json()).then(x => document.body.dataset.value = x.value);' +
  'const controller = new AbortController();' +
  'fetch("stream", {signal:controller.signal}).then(r => {' +
  'r.text().catch(e => document.body.dataset.abort = e.name); controller.abort(); });' +
  '</script></body></html>';

export default {
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === '/cases') return runCases(Number(url.searchParams.get('rounds') ?? 3));
    if (url.pathname !== '/') return new Response('not found', { status: 404 });

    let streamCanceled = false;
    const runtime = await createServoWorkerRuntime(servoWasm, {
      url: 'about:blank',
      fetchImpl: async (resourceUrl) => {
        if (resourceUrl === new URL('fixture.json', FIXTURE_URL).href) {
          return Response.json({ value: 'fetch-ok' });
        }
        if (resourceUrl === new URL('stream', FIXTURE_URL).href) {
          return new Response(new ReadableStream({
            start(controller) { controller.enqueue(new TextEncoder().encode('partial')); },
            cancel() { streamCanceled = true; },
          }));
        }
        throw new Error(`Unexpected smoke-test subrequest: ${resourceUrl}`);
      },
    });

    const bootstrap = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
    const loaded = runtime.loadHtml(FIXTURE_HTML, { url: FIXTURE_URL });
    const page = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
    runtime.evaluatePage(
      'document.title === "workerd fixture" && ' +
        'document.querySelector("#result")?.textContent === "served in workerd" && ' +
        'getComputedStyle(document.querySelector("#result")).color === "rgb(0, 128, 0)" && ' +
        'document.body.dataset.value === "fetch-ok" && ' +
        'document.body.dataset.abort === "AbortError" ? 42 : 0',
    );
    const evaluation = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
    const result = runtime.pageResult();
    const passed = loaded && bootstrap.settled && page.settled && evaluation.settled &&
      streamCanceled && result?.Ok?.Number === 42 && runtime.pendingFetchCount() === 0;
    return Response.json({ passed, result, streamCanceled,
      pendingFetches: runtime.pendingFetchCount() }, {
      status: passed ? 200 : 500,
    });
  },
};

// Runs the shared fixture corpus (DOM, CSS, canvas) repeatedly in real workerd,
// reporting results that arrive only after the adapter reported settlement and
// any trap or host stack exhaustion.
async function runCases(rounds) {
  const runtime = await createServoWorkerRuntime(servoWasm, {
    url: 'about:blank', fetchImpl: async () => new Response('{}'),
  });
  runtime.loadHtml('<!doctype html><body><p>cases</p></body>', { url: 'https://cases.example/' });
  await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
  const failures = [];
  let passed = 0;
  try {
    for (let round = 0; round < rounds; round++) {
      for (const { name, source } of webPlatformCases) {
        runtime.evaluatePage(`(${source}) ? 42 : 0`);
        const status = await runtime.pumpUntilSettled({ maxDurationMs: 3_000 });
        let result = runtime.pageResult();
        let lateTurns = 0;
        while (!result && lateTurns < 50) {
          runtime.pump();
          lateTurns++;
          result = runtime.pageResult();
        }
        if (result?.Ok?.Number === 42 && lateTurns === 0) passed++;
        else failures.push({ round, name, settled: status.settled, lateTurns, result });
      }
    }
  } catch (error) {
    failures.push({ fatal: `${error?.name}: ${error?.message}` });
  }
  return Response.json({ passed, failures }, { status: failures.length ? 500 : 200 });
}
