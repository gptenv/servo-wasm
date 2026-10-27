import assert from 'node:assert/strict';
import test from 'node:test';
import { installDocumentObserver, normalizeReadiness, waitForPageReadiness } from '../worker-adapter.mjs';

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


test('observer traverses open shadow roots and same-origin iframe documents', () => {
  const saved = Object.fromEntries(['document', 'location', 'MutationObserver']
    .map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
  const watched = [];
  const disconnected = [];
  const callbacks = [];
  class TestObserver {
    constructor(callback) { callbacks.push(callback); }
    observe(root, options) {
      watched.push({ root, options });
    }
    takeRecords() { return []; }
    disconnect() { disconnected.push(this); }
  }
  const element = (id, localName = 'div', children = []) => ({
    id, localName, children,
    matches(selector) { return selector === '#' + id; },
    querySelector(selector) {
      return this.querySelectorAll('*').find((child) => child.matches(selector)) ?? null;
    },
    querySelectorAll() {
      const flattened = [];
      const visit = (node) => {
        flattened.push(node);
        for (const nested of node.children ?? []) visit(nested);
      };
      for (const child of children) visit(child);
      return flattened;
    },
  });
  const shadowRoot = element('shadowRoot', 'shadow', [element('shadow-target')]);
  const frameRoot = element('frameRoot', 'document', [element('frame-target')]);
  const host = element('host');
  host.shadowRoot = shadowRoot;
  const frame = element('frame', 'iframe');
  frame.contentDocument = frameRoot;
  const main = element('main', 'document', [host, frame]);
  main.readyState = 'complete';
  const token = '__servoTestObserver';
  try {
    globalThis.document = main;
    globalThis.location = { href: 'https://example.com' };
    globalThis.MutationObserver = TestObserver;
    const initial = JSON.parse(installDocumentObserver(token));
    assert.equal(initial.observerSupported, true);
    assert.equal(initial.observedRoots, 3);
    assert.ok(watched.every(({ options }) => options.subtree && options.characterData && options.attributes));
    assert.equal(JSON.parse(globalThis[token].snapshot('#shadow-target')).selectorMatched, true);
    assert.equal(JSON.parse(globalThis[token].snapshot('#frame-target')).selectorMatched, true);
    callbacks[0]([]);
    assert.ok(JSON.parse(globalThis[token].snapshot()).mutations > initial.mutations);
    globalThis[token].dispose();
    assert.equal(disconnected.length, 3);
    assert.equal(globalThis[token], undefined);
  } finally {
    globalThis[token]?.dispose();
    for (const [name, desc] of Object.entries(saved)) {
      if (desc) Object.defineProperty(globalThis, name, desc);
      else delete globalThis[name];
    }
  }
});
