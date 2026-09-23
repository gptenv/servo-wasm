import servoWasm from '../../../target/wasm32-unknown-unknown/production-stripped/servo_js_wasm.wasm';
import { createServoWorkerRuntime } from '../worker-adapter.mjs';

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
