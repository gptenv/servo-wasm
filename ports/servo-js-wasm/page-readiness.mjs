/*
 * Browser-native page readiness for the Servo Worker adapter.
 *
 * Observers run inside the document realm. The host keeps driving Servo's
 * event loop and network queue; no arbitrary fixed sleep is treated as proof
 * of page readiness.
 */
export const DEFAULT_READINESS = Object.freeze({
  strategy: 'domquiet',
  quietMs: 400,
  networkIdleMs: 500,
  maxDurationMs: 10000,
  pollIntervalMs: 75,
});

const STRATEGIES = new Set(['none', 'commit', 'load', 'domquiet', 'networkidle']);
let nextObserverId = 0;

export function normalizeReadiness(options = {}) {
  if (options === null || typeof options !== 'object' || Array.isArray(options)) {
    throw new TypeError('Page readiness options must be an object');
  }
  const normalized = { ...DEFAULT_READINESS, ...options };
  if (!STRATEGIES.has(normalized.strategy)) {
    throw new RangeError('Readiness strategy must be none, commit, load, domquiet, or networkidle');
  }
  for (const key of ['quietMs', 'networkIdleMs', 'maxDurationMs', 'pollIntervalMs']) {
    if (!Number.isFinite(normalized[key]) || normalized[key] < 0) {
      throw new RangeError(key + ' must be a finite non-negative number');
    }
  }
  if (normalized.pollIntervalMs < 1 || normalized.maxDurationMs < 1) {
    throw new RangeError('pollIntervalMs and maxDurationMs must be positive');
  }
  if (normalized.selector !== undefined &&
      (typeof normalized.selector !== 'string' || !normalized.selector.trim() ||
       normalized.selector.length > 2048)) {
    throw new TypeError('selector must be a nonempty CSS selector of at most 2048 characters');
  }
  return normalized;
}

function decodePageEvaluation(value) {
  if (typeof value?.Ok?.String !== 'string') return null;
  try { return JSON.parse(value.Ok.String); } catch { return null; }
}

async function hostDelay(ms) {
  if (typeof globalThis.scheduler?.wait === 'function') {
    await globalThis.scheduler.wait(ms);
  } else {
    await new Promise((resolve) => setTimeout(resolve, ms));
  }
}

/** Runs in the page realm. This function is serialized, not executed in the host. */
function installDocumentObserver(token) {
  const previous = globalThis[token];
  if (previous && typeof previous.dispose === 'function') previous.dispose();

  const active = new Map();
  let lastMutation = performance.now();
  let changes = 0;
  let iframeCount = 0;
  let shadowCount = 0;
  const supported = typeof MutationObserver === 'function';
  const maximumRoots = 256;

  function changed() {
    lastMutation = performance.now();
    changes++;
  }

  function walk(root, onElement) {
    if (!root) return;
    if (supported && !active.has(root) && active.size < maximumRoots) {
      try {
        const observer = new MutationObserver(changed);
        observer.observe(root, {
          subtree: true, childList: true, attributes: true, characterData: true,
        });
        active.set(root, observer);
        // Newly discovered frame/shadow roots need a fresh quiet period.
        changed();
      } catch { /* Detached documents and unsupported node types. */ }
    }
    let elements;
    try { elements = root.querySelectorAll('*'); } catch { return; }
    for (const element of elements) {
      onElement?.(element);
      if (element.shadowRoot) {
        if (!active.has(element.shadowRoot)) shadowCount++;
        walk(element.shadowRoot, onElement);
      }
      if (element.localName === 'iframe') {
        try {
          const nested = element.contentDocument;
          if (nested) {
            if (!active.has(nested)) iframeCount++;
            walk(nested, onElement);
          }
        } catch { /* Cross-origin frames cannot be inspected. */ }
      }
    }
  }

  function snapshot(selector) {
    let matched = selector === undefined;
    try {
      if (selector !== undefined && document.querySelector(selector)) matched = true;
      walk(document, selector === undefined ? null : (element) => {
        if (matched) return;
        try { if (element.matches(selector)) matched = true; } catch { /* Invalid selector. */ }
      });
    } catch { /* Navigation may have detached the document. */ }

    // Take pending records synchronously in case Servo has not dispatched the
    // MutationObserver microtask yet.
    for (const observer of active.values()) {
      if (observer.takeRecords().length) changed();
    }
    return JSON.stringify({
      url: location.href,
      readyState: document.readyState,
      observerSupported: supported && active.size > 0,
      observedRoots: active.size,
      shadowRoots: shadowCount,
      iframes: iframeCount,
      mutations: changes,
      quietForMs: performance.now() - lastMutation,
      selectorMatched: matched,
    });
  }

  const handle = {
    snapshot,
    dispose() {
      for (const observer of active.values()) observer.disconnect();
      active.clear();
      if (globalThis[token] === handle) delete globalThis[token];
    },
  };
  globalThis[token] = handle;
  return snapshot();
}

function documentStatusScript(token, selector) {
  const key = JSON.stringify(token);
  return 'globalThis[' + key + ']?.snapshot(' + JSON.stringify(selector) + ')';
}

export async function navigationIdentity(runtime, maxDurationMs = 1500) {
  const value = decodePageEvaluation(await runtime.evaluate(
    'JSON.stringify({url: location.href, timeOrigin: performance.timeOrigin})',
    { maxDurationMs },
  ));
  return value && typeof value.url === 'string'
    ? { url: value.url, timeOrigin: typeof value.timeOrigin === 'number' ? value.timeOrigin : null }
    : { url: 'about:blank', timeOrigin: null };
}

/** Await document commit, including same-URL navigations. */
export async function waitForNavigationCommit(runtime, before, { maxDurationMs = 10000 } = {}) {
  const started = performance.now();
  let last = before;
  do {
    await runtime.pumpCooperatively({ maxTurns: 250 });
    const remaining = maxDurationMs - (performance.now() - started);
    if (remaining <= 0) break;
    last = await navigationIdentity(runtime, Math.min(remaining, 1000));
    const committed = before.timeOrigin !== null && last.timeOrigin !== null
      ? last.timeOrigin !== before.timeOrigin
      : last.url !== before.url;
    if (committed && last.url !== 'about:blank') {
      return { committed: true, url: last.url, elapsedMs: performance.now() - started };
    }
    await hostDelay(0);
  } while (performance.now() - started < maxDurationMs);
  return { committed: false, url: last.url, elapsedMs: performance.now() - started };
}

/**
 * Wait for a useful, document-level readiness signal, not a fixed delay.
 * The DOM observer covers open shadow roots and accessible iframe documents.
 * On engines without MutationObserver, network-idle is the fallback. On
 * continuously mutating pages, network-idle is a bounded fallback as well.
 */
export async function waitForPageReadiness(runtime, options = {}) {
  const config = normalizeReadiness(options);
  const started = performance.now();
  if (config.strategy === 'none' || config.strategy === 'commit') {
    return { ready: true, reason: config.strategy, elapsedMs: 0 };
  }

  const token = '__servoWorkerReadiness' + (++nextObserverId);
  const needDom = config.strategy !== 'networkidle';
  const script = '(' + installDocumentObserver.toString() + ')(' + JSON.stringify(token) + ')';
  let installed = false;
  let status = null;
  let idleSince = null;

  try {
    if (needDom) {
      const result = decodePageEvaluation(await runtime.evaluate(script, {
        maxDurationMs: Math.min(2000, config.maxDurationMs),
      }));
      installed = Boolean(result);
      status = result;
    }

    while (performance.now() - started < config.maxDurationMs) {
      await runtime.pumpCooperatively({ maxTurns: 200 });
      const networkBusy = runtime.networkActivity();
      idleSince = networkBusy === 0 ? (idleSince ?? performance.now()) : null;
      const networkQuietMs = idleSince === null ? 0 : performance.now() - idleSince;
      const remaining = config.maxDurationMs - (performance.now() - started);

      if (needDom && installed && remaining > 0) {
        status = decodePageEvaluation(await runtime.evaluate(
          documentStatusScript(token, config.selector), {
            maxDurationMs: Math.min(1000, remaining),
          },
        ));
      }
      const elapsedMs = performance.now() - started;
      const loaded = status?.readyState !== 'loading';
      const matched = config.selector === undefined || status?.selectorMatched === true;
      const observer = status?.observerSupported === true;
      const quiet = observer && (status?.quietForMs ?? 0) >= config.quietMs;
      const networkQuiet = networkQuietMs >= config.networkIdleMs;
      let reason = null;
      if (config.strategy === 'load' && status?.readyState === 'complete' && matched) {
        reason = 'load';
      } else if (config.strategy === 'networkidle' && networkQuiet && elapsedMs >= config.networkIdleMs) {
        reason = 'networkidle';
      } else if (config.strategy === 'domquiet' && loaded && matched) {
        if (quiet) reason = 'domquiet';
        else if (networkQuiet && (!observer ||
          elapsedMs >= Math.max(config.quietMs * 2, config.networkIdleMs * 2))) {
          reason = 'networkidle-fallback';
        }
      }
      if (reason !== null) {
        return { ready: true, reason, elapsedMs, networkBusy, ...status };
      }
      if (remaining <= 0) break;
      await hostDelay(Math.min(config.pollIntervalMs, remaining));
    }
    return {
      ready: false,
      reason: 'timeout',
      elapsedMs: performance.now() - started,
      networkBusy: runtime.networkActivity(),
      ...status,
    };
  } finally {
    if (installed) {
      try {
        await runtime.evaluate('globalThis[' + JSON.stringify(token) + ']?.dispose(); true', {
          maxDurationMs: 500,
        });
      } catch { /* Navigation may have replaced the observed document. */ }
    }
  }
}
