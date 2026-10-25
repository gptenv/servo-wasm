import assert from 'node:assert/strict';
import test from 'node:test';
import { normalizeReadiness, waitForPageReadiness } from '../worker-adapter.mjs';

const status = (overrides = {}) => ({
  observerSupported: true,
  quietForMs: 2000,
  readyState: 'complete',
  selectorMatched: true,
  observedRoots: 3,
  mutations: 2,
  url: 'https://example.com/',
  ...overrides,
});

function fakePage({ statusResult = status(), busy = () => 0 } = {}) {
  const calls = [];
  return {
    calls,
    networkActivity: busy,
    async pumpCooperatively() { calls.push('pump'); },
    async evaluate(script) {
      calls.push(script.includes('dispose()') ? 'cleanup' : 'evaluate');
      return { Ok: { String: JSON.stringify(statusResult) } };
    },
  };
}

test('readiness rejects invalid settings before touching Servo', () => {
  assert.throws(() => normalizeReadiness({ strategy: 'sleep' }), /strategy/);
  assert.throws(() => normalizeReadiness({ quietMs: -1 }), /quietMs/);
  assert.throws(() => normalizeReadiness({ pollIntervalMs: 0 }), /pollIntervalMs/);
  assert.throws(() => normalizeReadiness({ selector: '' }), /selector/);
});

test('DOM quiet succeeds and disconnects the observer', async () => {
  const runtime = fakePage();
  const result = await waitForPageReadiness(runtime, {
    quietMs: 10, networkIdleMs: 10, maxDurationMs: 250, pollIntervalMs: 5,
  });
  assert.equal(result.ready, true);
  assert.equal(result.reason, 'domquiet');
  assert.ok(runtime.calls.includes('cleanup'));
});

test('network-idle fallback works without MutationObserver', async () => {
  const runtime = fakePage({ statusResult: status({ observerSupported: false }) });
  const result = await waitForPageReadiness(runtime, {
    quietMs: 10, networkIdleMs: 15, maxDurationMs: 250, pollIntervalMs: 5,
  });
  assert.equal(result.ready, true);
  assert.equal(result.reason, 'networkidle-fallback');
});

test('network-idle strategy waits until pending requests drain', async () => {
  let checks = 0;
  const runtime = fakePage({ busy: () => (++checks < 4 ? 2 : 0) });
  const result = await waitForPageReadiness(runtime, {
    strategy: 'networkidle', networkIdleMs: 15, maxDurationMs: 250, pollIntervalMs: 5,
  });
  assert.equal(result.ready, true);
  assert.equal(result.reason, 'networkidle');
});

test('unmatched selector and loading state cannot be reported ready', async () => {
  for (const overridden of [{ selectorMatched: false }, { readyState: 'loading' }]) {
    const runtime = fakePage({ statusResult: status(overridden) });
    const result = await waitForPageReadiness(runtime, {
      selector: '.target', quietMs: 0, networkIdleMs: 0, maxDurationMs: 30, pollIntervalMs: 5,
    });
    assert.equal(result.ready, false);
    assert.equal(result.reason, 'timeout');
    assert.ok(runtime.calls.includes('cleanup'));
  }
});

test('none and commit modes perform no page evaluations', async () => {
  const runtime = fakePage();
  assert.equal((await waitForPageReadiness(runtime, { strategy: 'none' })).ready, true);
  assert.equal((await waitForPageReadiness(runtime, { strategy: 'commit' })).ready, true);
  assert.deepEqual(runtime.calls, []);
});
