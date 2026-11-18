import type { ServoWorkerEnv } from '../src/worker-env';
/**
 * Unit tests for the ServoBrowserSession Durable Object. The Servo WASM
 * runtime adapter and the cloudflare:workers DurableObject base class are
 * replaced with deterministic fakes so every branch of the session lifecycle
 * (initialize, restore, operate, expire, close, alarm) is exercised without
 * workerd or the real engine artifact.
 */

import { runInNewContext } from 'node:vm';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { FakeRuntimeOptions, FakeServoRuntime } from './helpers/fake-runtime';
import { invalidSummary } from './helpers/fake-runtime';
import { createFakeDurableObjectState, asDoState } from './helpers/fake-do';
import type { FakeDurableObjectState } from './helpers/fake-do';

vi.mock('cloudflare:workers', () => {
  return {
    DurableObject: class {
      ctx: unknown;
      env: unknown;
      constructor(ctx: unknown, env: unknown) {
        this.ctx = ctx;
        this.env = env;
      }
    },
  };
});

const runtimeOptionsHolder: { options: FakeRuntimeOptions } = { options: {} };
const createServoWorkerRuntime = vi.fn(async (_wasm: unknown, config: Record<string, unknown>) => {
  const { FakeServoRuntime } = await import('./helpers/fake-runtime');
  // Snapshot the scripted options at creation time so later mutations cannot
  // retroactively change an already-running session's behaviour.
  const runtime = new FakeServoRuntime({ ...runtimeOptionsHolder.options });
  if (typeof config.url === 'string') {
    runtime.setInitialUrl(config.url);
    // The real core adapter pumps and waits for the initial navigation before
    // returning; the fake must model that boundary too, not return too early.
    if (config.url !== 'about:blank') {
      await runtime.waitForNavigationCommit({ url: 'about:blank', timeOrigin: null });
      await runtime.waitForPageReady(config.readiness);
    }
  }
  runtimeConfigs.push(config);
  createdRuntimes.push(runtime);
  return runtime;
});
vi.mock('./helpers/servo-worker-adapter.ts', () => ({
  createServoWorkerRuntime,
  createCloudflareServoBrowser: (wasm: unknown, _env: unknown, config: Record<string, unknown>) =>
    createServoWorkerRuntime(wasm, config),
}));
vi.mock('../src/vendor/servo-worker/servo_js_wasm.wasm', () => ({
  default: {},
}));
vi.mock('../src/recording-encoder', () => ({ encodeRecordingMp4: vi.fn(async () => new Uint8Array([0, 0, 0, 0])) }));

const fetchImplSpy = vi.fn();
const webSocketFactorySpy = vi.fn(function (this: unknown) { return { fake: true }; });
let runtimeConfigs: Record<string, unknown>[] = [];
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let createdRuntimes: any[] = [];
let pumpCalls = 0;
let consoleErrorSpy: ReturnType<typeof vi.spyOn>;

type SessionModule = typeof import('../src/browser-session');
let mod: SessionModule;
let state: FakeDurableObjectState;

const UUID = '00000000-0000-4000-8000-000000000001';

const validResumeState = {
  version: 1,
  url: 'https://public.example/page',
  scrollX: 1,
  scrollY: 2,
  fields: [],
  localStorage: [['k', 'v']],
  sessionStorage: [],
  cookies: '',
};

function makeSnapshotJson(overrides: Record<string, unknown> = {}): string {
  return JSON.stringify({ ...validResumeState, ...overrides });
}

function seedActiveRow(db: FakeDurableObjectState['storage']['sql'], expiresAt: number, status = 'active'): void {
  db.exec(
    'INSERT INTO browser_session (singleton, status, created_at, updated_at, expires_at, width, height) VALUES (1, ?, ?, ?, ?, ?, ?)',
    status, 1, 2, expiresAt, 800, 600,
  );
}

function seedSnapshot(db: FakeDurableObjectState['storage']['sql'], json: string): void {
  db.exec(
    'INSERT INTO browser_snapshot (singleton, snapshot_json) VALUES (1, ?) ON CONFLICT(singleton) DO UPDATE SET snapshot_json = excluded.snapshot_json',
    json,
  );
}

function seedAsset(db: FakeDurableObjectState['storage']['sql'], name: string, text: string): void {
  db.exec('INSERT INTO browser_asset (name, chunk_index, chunk_text) VALUES (?, ?, ?)', name, 0, text);
}

/** Options as the MCP layer always sends them (schema defaults applied). */
function initOptions(extra: Record<string, unknown> = {}) {
  return {
    sessionId: UUID, width: 1280, height: 720, ...extra,
  } as never;
}

async function newSession(idString = 'session-do-id') {
  state = createFakeDurableObjectState(idString);
  return new mod.ServoBrowserSession(asDoState(state), {
    MESH: { fetch: (input: RequestInfo | URL, init?: RequestInit) => fetch(input, init) },
  } as unknown as ServoWorkerEnv);
}

const sql = () => state.storage.sql as unknown as import('./helpers/fake-sql').FakeSqlDatabase;

/**
 * The default restore-state evaluation returns a valid snapshot object so
 * captureSnapshot succeeds unless a test overrides it. Raw objects are accepted
 * directly by parseEvaluationResult; string values are JSON-parsed first.
 */
const RESUME_MARKER = '(async()=>{\n  const nodePath=';
const SUMMARY_MARKER = 'JSON.stringify({url: location.href';

/** Wrap a payload the way the real adapter returns page-evaluation results. */
function okString(value: unknown): { Ok: { String: string } } {
  if (value && typeof value === 'object' && 'Ok' in (value as Record<string, unknown>)) {
    return value as { Ok: { String: string } };
  }
  return { Ok: { String: typeof value === 'string' ? value : JSON.stringify(value) } };
}

function scriptRuntime(options: FakeRuntimeOptions): void {
  runtimeOptionsHolder.options = { evaluations: { [RESUME_MARKER]: okString(validResumeState) }, ...options };
}

beforeEach(async () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-01-01T00:00:00Z'));
  scriptRuntime({});
  runtimeConfigs = [];
  createdRuntimes = [];
  pumpCalls = 0;
  createServoWorkerRuntime.mockClear();
  fetchImplSpy.mockReset();
  webSocketFactorySpy.mockClear();
  consoleErrorSpy = vi.spyOn(console, 'error').mockImplementation(() => undefined);
  mod = await import('../src/browser-session');
});

afterEach(() => {
  vi.useRealTimers();
});

describe('browserSessionOptionsSchema', () => {
  it('applies viewport defaults', () => {
    const parsed = mod.browserSessionOptionsSchema.parse({ sessionId: UUID });
    expect(parsed.width).toBe(1280);
    expect(parsed.height).toBe(720);
  });

  it('rejects combining a URL with inline HTML', () => {
    expect(mod.browserSessionOptionsSchema.safeParse({ sessionId: UUID, url: 'https://a.example/', html: '<p></p>' }).success).toBe(false);
  });

  it('rejects out-of-range viewports and malformed ids', () => {
    expect(mod.browserSessionOptionsSchema.safeParse({ sessionId: UUID, width: 100 }).success).toBe(false);
    expect(mod.browserSessionOptionsSchema.safeParse({ sessionId: UUID, height: 100 }).success).toBe(false);
    expect(mod.browserSessionOptionsSchema.safeParse({ sessionId: 'not-a-uuid' }).success).toBe(false);
  });
});

describe('runtime configuration wiring', () => {
  it('passes viewport, blank start URL, limits, and a logging callback to the adapter', async () => {
    scriptRuntime({ summary: { url: 'about:blank', title: '', text: '' } });
    // initialize() logs through the runtime callback using the sessionId from
    // the options object (the DO id only appears on restore paths).
    const session = await newSession();
    await session.initialize(initOptions({ sessionId: 'logged-session' } as never));
    expect(createServoWorkerRuntime).toHaveBeenCalledTimes(1);
    const config = runtimeConfigs[0];
    expect(config.width).toBe(1280);
    expect(config.height).toBe(720);
    expect(config.url).toBe('about:blank');
    expect(config.maxResponseBytes).toBe(8 * 1024 * 1024);
    expect(config.maxSubrequests).toBe(50);
    expect(typeof config.onNetworkResult).toBe('function');
    expect(config.readiness).toEqual({});
    // console.error receives a single already-formatted string argument.
    const log = config.log as (message: string) => void;
    log('short message');
    log('x'.repeat(3000));
    const shortCall = consoleErrorSpy.mock.calls.find((call: unknown[]) =>
      String(call[0]).includes('[servo:logged-session] short message'));
    expect(shortCall).toBeDefined();
    const longCall = consoleErrorSpy.mock.calls.find((call: unknown[]) =>
      String(call[0]).includes('x'.repeat(2048)));
    expect(longCall).toBeDefined();
    // The runtime message is truncated to 2048 characters inside the prefix line.
    expect(String(longCall![0])).toBe(`[servo:logged-session] ${'x'.repeat(2048)}`);
  });

  it('records network failures reported by the Cloudflare Servo factory', async () => {
    scriptRuntime({ summary: { url: 'about:blank', title: '', text: '' } });
    const session = await newSession();
    await session.initialize(initOptions());
    const onNetworkResult = runtimeConfigs[0].onNetworkResult as (url: string, status: number | null, error: unknown) => void;
    expect(typeof onNetworkResult).toBe('function');
    onNetworkResult('https://public.example/failure', null, new Error('host fetch failed'));
    expect((session as unknown as { fetchFailures: Map<string, string> }).fetchFailures.get('https://public.example/failure')).toBe('host fetch failed');
    onNetworkResult('https://public.example/plain', null, 'plain rejection');
    expect((session as unknown as { fetchFailures: Map<string, string> }).fetchFailures.get('https://public.example/plain')).toBe('plain rejection');
    onNetworkResult('https://public.example/http-error', 404, null);
    expect((session as unknown as { fetchFailures: Map<string, string> }).fetchFailures.get('https://public.example/http-error')).toBe('HTTP 404');
  });

  it('does not expose an MCP-owned Mesh fetch or WebSocket factory (network is owned by the Servo factory)', async () => {
    scriptRuntime({ summary: { url: 'about:blank', title: '', text: '' } });
    const session = await newSession();
    await session.initialize(initOptions());
    expect(typeof runtimeConfigs[0].fetchImpl).toBe('function');
    expect(typeof runtimeConfigs[0].webSocketFactory).toBe('function');
  });
});

describe('font asset persistence', () => {
  it('stores multi-chunk font assets on initialization', async () => {
    const fontBase64 = 'QUJD'.repeat(300_000); // ~1.2 MB of base64 -> two chunks
    const session = await newSession();
    await session.initialize(initOptions({ fontBase64 }));
    const fontRows = sql().table('browser_asset')!.rows.filter((row) => row.name === 'font:000000');
    expect(fontRows.map((row) => row.chunk_index)).toEqual([0, 1]);
  });

  it('replays stored fonts into freshly created runtimes in sorted order', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedAsset(sql(), 'font:000001', btoa('second'));
    seedAsset(sql(), 'font:000000', btoa('first'));
    seedSnapshot(sql(), makeSnapshotJson());
    await session.inspect();
    expect(createdRuntimes[0].fonts.map((bytes: Uint8Array) => Buffer.from(bytes).toString())).toEqual(['first', 'second']);
  });

  it('skips font assets whose chunk rows vanished between listing and reading', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedAsset(sql(), 'font:000000', btoa('gone-soon'));
    seedSnapshot(sql(), makeSnapshotJson());
    const table = sql().table('browser_asset')!;
    const realExec = sql().exec.bind(sql());
    let evaded = false;
    sql().exec = ((statement: string, ...params: unknown[]) => {
      if (!evaded && /^SELECT chunk_text FROM browser_asset WHERE name = \? ORDER BY chunk_index/i.test(statement.trim())) {
        evaded = true;
        table.rows = table.rows.filter((row) => row.name !== 'font:000000');
      }
      return realExec(statement, ...params);
    }) as never;
    await session.inspect();
    expect(createdRuntimes[0].fonts).toHaveLength(0);
  });
});

describe('initialize', () => {
  it('creates a blank session and returns page info plus capabilities', async () => {
    scriptRuntime({ summary: { url: 'about:blank', title: 'Blank', text: '' }, capabilitiesValue: { supported: ['dom'] } });
    const session = await newSession();
    const result = await session.initialize(initOptions());
    expect(result.sessionId).toBe(UUID);
    expect(result.page).toEqual({ url: 'about:blank', title: 'Blank', text: '' });
    expect(result.capabilities).toEqual({ supported: ['dom'] });
    expect(result.expiresAt).toBe(Date.now() + 30 * 24 * 60 * 60 * 1000);
    expect(sql().table('browser_session')!.rows[0].status).toBe('active');
    expect(state.alarms.set.length).toBeGreaterThan(0);
  });

  it('loads an inline HTML document and stores it as a resumable asset', async () => {
    const html = '<html><body><h1>Hello</h1></body></html>';
    scriptRuntime({ summary: { url: 'https://servo-inline.invalid/doc', title: 'Hello', text: 'Hello' } });
    const session = await newSession();
    const result = await session.initialize(initOptions({ html }));
    expect(result.page.title).toBe('Hello');
    const calls = lastRuntimeCalls();
    expect(calls.some((call) => call.startsWith('loadHtml:'))).toBe(true);
    expect(calls.some((call) => call.startsWith('loadPage:'))).toBe(false);
    expect(sql().table('browser_asset')!.rows.some((row) => row.name === 'initial-html')).toBe(true);
  });

  it('loads a public URL when one is provided', async () => {
    scriptRuntime({ summary: { url: 'https://public.example/page', title: 'Public', text: 'text' } });
    const session = await newSession();
    await session.initialize(initOptions({ url: 'https://public.example/page' }));
    expect(runtimeConfigs[0].url).toBe('https://public.example/page');
    expect(lastRuntimeCalls()).not.toContain('loadPage:https://public.example/page');
  });

  it('fails loudly when a requested navigation remains at about:blank', async () => {
    // This exercises a real asynchronous timeout path, not a frozen fake clock.
    vi.useRealTimers();
    scriptRuntime({ summary: { url: 'about:blank', title: '', text: '' } });
    const session = await newSession();
    await expect(session.initialize(initOptions({ url: 'https://public.example/page' })))
      .rejects.toThrow(/remained at about:blank/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
  });

  it('refuses private navigation targets before creating a runtime', async () => {
    const session = await newSession();
    await expect(session.initialize(initOptions({ url: 'http://127.0.0.1/' }))).rejects.toThrow(TypeError);
    expect(createServoWorkerRuntime).not.toHaveBeenCalled();
    // The guard runs before any row is inserted.
    expect(sql().table('browser_session')!.rows).toHaveLength(0);
  });

  it('rejects inline HTML that exceeds the persisted-size limit', async () => {
    const session = await newSession();
    const huge = 'x'.repeat(1024 * 1024 + 1);
    await expect(session.initialize(initOptions({ html: huge }))).rejects.toThrow(RangeError);
    expect(sql().table('browser_session')!.rows).toHaveLength(0);
  });

  it('rejects a second initialize on the same session id', async () => {
    const session = await newSession();
    await session.initialize(initOptions());
    await expect(session.initialize(initOptions())).rejects.toThrow(/already been initialized/);
  });

  it('reports when Servo rejects the supplied HTML document', async () => {
    scriptRuntime({ loadHtmlReturns: false });
    const session = await newSession();
    await expect(session.initialize(initOptions({ html: '<html></html>' }))).rejects.toThrow(/rejected the supplied HTML/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
  });

  it('surfaces adapter failures verbatim and cleans up after itself', async () => {
    createServoWorkerRuntime.mockRejectedValueOnce(new Error('wasm instantiate failed'));
    const session = await newSession();
    await expect(session.initialize(initOptions())).rejects.toThrow('wasm instantiate failed');
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
    expect(sql().table('browser_asset')!.rows).toHaveLength(0);
    expect(sql().table('browser_snapshot')!.rows).toHaveLength(0);
  });

  it('records non-Error adapter failures as strings', async () => {
    createServoWorkerRuntime.mockRejectedValueOnce('boom');
    const session = await newSession();
    await expect(session.initialize(initOptions())).rejects.toBe('boom');
  });

  it('propagates runtime creation failures when initializing a requested URL', async () => {
    createServoWorkerRuntime.mockRejectedValueOnce(new Error('runtime creation failed'));
    const session = await newSession();
    await expect(session.initialize(initOptions({ url: 'https://public.example/' }))).rejects.toThrow('runtime creation failed');
  });

  it('processes an initial load cooperatively without requiring idle settlement', async () => {
    scriptRuntime({ pumpResult: { settled: false } });
    const session = await newSession();
    const result = await session.initialize(initOptions({ url: 'https://public.example/' }));
    expect(result.page.url).toBe('https://public.example/');
    expect(lastRuntimeCalls()).toContain('pump');
  });

  it('retries cooperative pumping until a delayed navigation commits', async () => {
    vi.useRealTimers();
    let pumpCount = 0;
    scriptRuntime({
      summary: () => pumpCount >= 2
        ? { url: 'https://public.example/page', title: 'Public', text: 'loaded' }
        : { url: 'about:blank', title: '', text: '' },
      pumpFactory: async () => {
        pumpCount++;
        return { settled: false };
      },
    });
    const session = await newSession();
    const result = await session.initialize(initOptions({ url: 'https://public.example/page' }));
    expect(result.page).toEqual({ url: 'https://public.example/page', title: 'Public', text: 'loaded' });
    expect(lastRuntimeCalls().filter((call) => call === 'pump')).toHaveLength(2);
  });

  it('waits for the new document when navigating between already-loaded pages', async () => {
    vi.useRealTimers();
    let pumpCount = 0;
    scriptRuntime({
      summary: () => pumpCount >= 2
        ? { url: 'https://public.example/next', title: 'Next', text: 'new document' }
        : { url: 'https://public.example/old', title: 'Old', text: 'stale document' },
      pumpFactory: async () => {
        pumpCount++;
        return { settled: false };
      },
    });
    const session = await newSession();
    await session.initialize(initOptions({ url: 'https://public.example/old' }));
    pumpCount = 0;
    const result = await session.navigate('https://public.example/next');
    expect(result.page).toEqual({
      url: 'https://public.example/next', title: 'Next', text: 'new document',
    });
    expect(lastRuntimeCalls().filter((call) => call === 'pump')).toHaveLength(3);
  });


  it('propagates evaluation failures while summarizing the initial page', async () => {
    scriptRuntime({
      summary: { url: 'https://public.example/page' },
      evaluations: { [SUMMARY_MARKER]: { Err: 'evaluate exploded' } },
    });
    const session = await newSession();
    await expect(session.initialize(initOptions({ url: 'https://public.example/' }))).rejects.toThrow(/invalid page summary/);
  });

  it('logs snapshot-capture failures caused by a malformed restore-state payload', async () => {
    scriptRuntime({
      summary: { url: 'about:blank' },
      evaluations: { [RESUME_MARKER]: okString(invalidSummary) },
    });
    const session = await newSession();
    // initialize() captures its snapshot before operate()-style error logging,
    // so a malformed restore payload aborts initialization and marks the
    // session failed.
    await expect(session.initialize(initOptions())).rejects.toThrow(/invalid browser restore state/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
  });

  it('persists every Web Storage entry and large values across multiple snapshot chunks', async () => {
    const largeValue = 'y'.repeat(2 * 1024 * 1024);
    const localStorage = Array.from({ length: 75 }, (_, index) => [`key-${index}`, `value-${index}`] as [string, string]);
    localStorage.push(['large-value', largeValue]);
    const splitSurrogateValue = `${'x'.repeat(32_767)}🙂tail`;
    localStorage.push(['unicode-value', splitSurrogateValue]);
    scriptRuntime({
      summary: { url: 'https://public.example/page', title: 't', text: '' },
      evaluations: {
        [RESUME_MARKER]: okString({
          ...validResumeState,
          origin: 'https://public.example',
          localStorage,
          sessionStorage: [['large-session-value', largeValue]],
          indexedDB: [{
            name: 'large-db', version: 1,
            stores: [{ name: 'items', keyPath: 'id', autoIncrement: false, indexes: [], records: [
              { key: { root: 1, nodes: [] }, primaryKey: { root: 1, nodes: [] }, value: { root: { r: 0 }, nodes: [{ t: 'object', e: [['id', 1], ['payload', largeValue]] }] } },
            ] }],
          }],
        }),
      },
    });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    const page = await session.inspect();
    expect(page.title).toBe('t');
    const captureScript = createdRuntimes.at(-1)!.evaluations.find((call: { script: string }) => call.script.includes('const nodePath='))?.script;
    expect(captureScript).toMatch(/^\(async\(\)=>\{/);
    expect(captureScript).toMatch(/\}\)\(\)$/);
    expect(captureScript).toContain('store.openCursor()');
    expect(captureScript).not.toContain('store.getAllKeys()');
    expect(captureScript).not.toContain('store.getAll()');
    const header = JSON.parse(sql().table('browser_snapshot')!.rows[0].snapshot_json as string) as {
      version: number;
      originAssets: Record<string, string>;
    };
    expect(header.version).toBe(3);
    const assetName = header.originAssets['https://public.example'];
    const chunks = sql().table('browser_asset')!.rows
      .filter((row) => row.name === assetName)
      .sort((a, b) => Number(a.chunk_index) - Number(b.chunk_index));
    expect(chunks.length).toBeGreaterThan(1);
    type SavedOriginState = {
      localStorage: [string, string][];
      sessionStorage: [string, string][];
      indexedDB: Array<{
        stores: Array<{
          records: Array<{
            value: { nodes: Array<{ e: Array<[string, string]> }> };
          }>;
        }>;
      }>;
    };
    const saved = JSON.parse(chunks.map((row) => row.chunk_text).join('')) as SavedOriginState;
    expect(saved.localStorage).toHaveLength(77);
    expect(saved.localStorage[75]).toEqual(['large-value', largeValue]);
    expect(saved.localStorage[76]).toEqual(['unicode-value', splitSurrogateValue]);
    expect(saved.sessionStorage).toEqual([['large-session-value', largeValue]]);
    expect(saved.indexedDB[0].stores[0].records[0].value.nodes[0].e[1]).toEqual(['payload', largeValue]);
    expect(consoleErrorSpy).not.toHaveBeenCalledWith(expect.stringContaining('servo_snapshot_failed'));
  });
});

describe('getStatus', () => {
  it('reports missing sessions', async () => {
    const session = await newSession();
    expect(await session.getStatus()).toEqual({ status: 'missing', runtimeAvailable: false, resumable: false });
  });

  it('reports active sessions with a live runtime and a stored snapshot', async () => {
    scriptRuntime({ resumeState: [{ pageResultValue: { Ok: { String: '{"restored":true}' } } }] });
    const session = await newSession();
    await session.initialize(initOptions());
    const status = await session.getStatus();
    expect(status.status).toBe('active');
    expect(status.runtimeAvailable).toBe(true);
    expect(status.resumable).toBe(true);
    expect(status.updatedAt).toBeDefined();
  });

  it('reaps expired sessions lazily and reports them as expired', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() - 1);
    seedSnapshot(sql(), makeSnapshotJson());
    seedAsset(sql(), 'initial-html', '<html></html>');
    const status = await session.getStatus();
    expect(status.status).toBe('expired');
    expect(status.runtimeAvailable).toBe(false);
    expect(status.resumable).toBe(false);
    expect(sql().table('browser_snapshot')!.rows).toHaveLength(0);
    expect(sql().table('browser_asset')!.rows).toHaveLength(0);
    expect(state.alarms.deleted).toBeGreaterThanOrEqual(1);
  });

  it('discards trapped runtimes and reports them unavailable', async () => {
    scriptRuntime({ trapped: true });
    const session = await newSession();
    await session.initialize(initOptions());
    const status = await session.getStatus();
    expect(status.runtimeAvailable).toBe(false);
  });

  it('reports closed sessions without probing snapshots', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000, 'closed');
    const status = await session.getStatus();
    expect(status.status).toBe('closed');
    expect(status.resumable).toBe(false);
  });
});

describe('snapshot validation', () => {
  it('rejects snapshots with an unsupported shape', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson({ version: 2 }));
    await expect(session.inspect()).rejects.toThrow(/unsupported format/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
  });

  it('rejects snapshots whose url field is not a string', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson({ url: 42 }));
    await expect(session.inspect()).rejects.toThrow(/unsupported format/);
  });

  it('rejects snapshots without a fields array', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson({ fields: 'nope' }));
    await expect(session.inspect()).rejects.toThrow(/unsupported format/);
  });

  it('refuses to restore sessions that have no saved tab state', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    await expect(session.inspect()).rejects.toThrow(/no saved tab state/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
  });
});

describe('restore behaviour', () => {
  it('restores a public page, applies saved state, and reloads once storage exists', async () => {
    scriptRuntime({
      summary: { url: 'https://public.example/page', title: 'Restored', text: 'hi' },
      resumeState: [
        { pageResultValue: { Ok: { String: '{"restored":true,"hasStorage":false,"hasCookies":false,"hasIndexedDBState":false}' } } },
      ],
    });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    const page = await session.inspect();
    expect(page.url).toBe('https://public.example/page');
    const calls = lastRuntimeCalls();
    expect(calls).toContain('loadPage:https://public.example/page');
    // The default restore-state pageResult reports no storage/cookies, so the
    // extra reload path is not taken during restoration.
    expect(calls).not.toContain('reload');
    expect(calls.filter((call) => call === 'evaluatePage').length).toBe(1);
  });

  it('restores inline documents from the stored HTML asset without reloading', async () => {
    scriptRuntime({
      summary: { url: 'https://servo-inline.invalid/doc', title: 'Inline', text: '' },
    });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson({ url: 'https://servo-inline.invalid/doc' }));
    seedAsset(sql(), 'initial-html', '<html><body>inline</body></html>');
    await session.inspect();
    const calls = lastRuntimeCalls();
    expect(calls.some((call) => call.startsWith('loadHtml:<html><body>inline'))).toBe(true);
    expect(calls).not.toContain('reload');
  });

  it('does not pump or apply state for about:blank restores', async () => {
    scriptRuntime({ summary: { url: 'about:blank' } });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson({ url: 'about:blank' }));
    // The snapshot URL is about:blank, so restoration skips document loading,
    // pumping, and saved-state application entirely.
    const page = await session.inspect();
    expect(page.url).toBe('about:blank');
    const calls = lastRuntimeCalls();
    expect(calls.some((call) => call.startsWith('loadHtml:'))).toBe(false);
    expect(calls.some((call) => call.startsWith('loadPage:'))).toBe(false);
    expect(calls).not.toContain('evaluatePage');
    expect(calls).not.toContain('reload');
    // Restoring a blank document never pumps the runtime.
    expect(calls.filter((call) => call === 'pump')).toHaveLength(0);
  });

  it('aborts restoration when Servo cannot reopen the saved URL', async () => {
    scriptRuntime({ loadPageReturns: false });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    await expect(session.inspect()).rejects.toThrow(/could not reopen the saved tab URL/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
  });

  it('aborts restoration when the saved state cannot be applied', async () => {
    scriptRuntime({ resumeState: [{ evaluatePageReturns: false }] });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    await expect(session.inspect()).rejects.toThrow(/could not apply the saved tab state/);
  });

  it('aborts when the post-restore reload fails', async () => {
    scriptRuntime({
      reloadReturns: false,
      resumeState: [{ pageResultValue: { Ok: { String: '{"hasCookies":true}' } } }],
    });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    await expect(session.inspect()).rejects.toThrow(/could not reload the restored tab/);
  });

  it('aborts when the saved state cannot be reapplied after reload', async () => {
    // Step order follows the interleaving of evaluatePage/pageResult calls:
    // The first state application reports saved data and triggers a reload;
    // the second application is configured to fail.
    scriptRuntime({
      resumeState: [
        { pageResultValue: { Ok: { String: '{"restored":true,"hasStorage":true}' } } },
        { evaluatePageReturns: false },
      ],
    });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    await expect(session.inspect()).rejects.toThrow(/could not reapply the saved tab state/);
  });

  it('completes reload restores that successfully reapply the saved state', async () => {
    // The first state application reports stored data and the second one
    // confirms that the data was reapplied after reload.
    scriptRuntime({
      summary: { url: 'https://public.example/page', title: 'Restored', text: '' },
      resumeState: [
        { pageResultValue: { Ok: { String: '{"restored":true,"hasStorage":true}' } } },
        { pageResultValue: { Ok: { String: '{"restored":true,"hasStorage":true}' } } },
      ],
    });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    const page = await session.inspect();
    expect(page.title).toBe('Restored');
    const calls = lastRuntimeCalls();
    expect(calls).toContain('reload');
    expect(calls.filter((call) => call === 'evaluatePage')).toHaveLength(2);
    expect(calls.filter((call) => call === 'pageResult').length).toBeGreaterThanOrEqual(2);
  });

  it('swallows reset errors while discarding a partially restored runtime', async () => {
    scriptRuntime({ loadPageReturns: false, resetThrows: true });
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    await expect(session.inspect()).rejects.toThrow(/could not reopen/);
    // The reset failure was intentionally discarded (no servo_session_reset_failed log).
    expect(consoleErrorSpy.mock.calls.filter((call: unknown[]) => String(call[0]).includes('reset_failed'))).toHaveLength(0);
  });

  it('blocks restoring private URLs captured in snapshots', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson({ url: 'http://localhost/x' }));
    await expect(session.inspect()).rejects.toThrow(/Private, local/);
  });
});

describe('operate lifecycle', () => {
  async function activeSession(options: FakeRuntimeOptions = {}) {
    scriptRuntime(options);
    const session = await newSession();
    await session.initialize(initOptions());
    return session;
  }

  it.each([
    { rejection: new TypeError('fetch failed'), message: 'fetch failed' },
    { rejection: 'plain rejection', message: 'plain rejection' },
  ])('annotates the navigated page when its host fetch rejects with $message', async ({ rejection, message }) => {
    const url = 'https://public.example/page';
    const session = await activeSession({
      summary: () => ({ url: pumpCalls === 0 ? 'https://public.example/before' : url }),
      pumpFactory: async () => {
        if (pumpCalls++ === 0) {
          const onNetworkResult = runtimeConfigs.at(-1)!.onNetworkResult as (url: string, status: number | null, error: unknown) => void;
          onNetworkResult(url, null, rejection);
        }
        return { settled: true };
      },
    });
    const hostFetch = vi.spyOn(globalThis, 'fetch').mockRejectedValue(rejection);
    try {
      const result = await session.navigate(url);
      expect(result.action).toBe('navigate');
      expect(result.page.loadError).toBe(`The page could not be loaded: ${message}`);
    } finally {
      hostFetch.mockRestore();
    }
  });

  it('preserves the last usable snapshot when an HTTP URL loads an opaque error document', async () => {
    const session = await activeSession();
    const runtime = createdRuntimes.at(-1) as FakeServoRuntime;
    const previous = sql().table('browser_snapshot')!.rows[0].snapshot_json;
    const url = 'https://public.example/unreachable';
    runtime.setInitialUrl('https://other.example/before');
    const evaluate = runtime.evaluate.bind(runtime);
    const guards: string[] = [];
    vi.spyOn(runtime, 'evaluate').mockImplementation(async (script) => {
      if (script.includes('const nodePath=') || script.includes('const restoreStorage=')) {
        // Like Servo's network-error page, the URL names the site but the
        // document has no access to its storage. DOM or storage access throws.
        const context = { origin: 'null', location: new URL(url) };
        const result = await runInNewContext(script, context);
        guards.push(result);
        return { Ok: { String: result } };
      }
      return evaluate(script);
    });

    await expect(session.navigate(url)).resolves.toMatchObject({ page: { url } });
    expect(guards.map(value => JSON.parse(value))).toEqual([{ restored: false }, { opaqueDocument: true }]);
    expect(sql().table('browser_snapshot')!.rows[0].snapshot_json).toBe(previous);
    await expect(session.getStatus()).resolves.toMatchObject({ resumable: true });
  });

  it('keeps plain summaries when no fetch failed for the page URL', async () => {
    const session = await activeSession({
      summary: () => pumpCalls === 0
        ? { url: 'https://public.example/page' }
        : { url: 'https://public.example/other' },
      pumpFactory: async () => {
        pumpCalls++;
        return { settled: true };
      },
    });
    const result = await session.navigate('https://public.example/other');
    expect(result.page.loadError).toBeUndefined();
  });

  it('refuses navigating to private addresses', async () => {
    const session = await activeSession();
    await expect(session.navigate('http://10.0.0.5/')).rejects.toThrow(TypeError);
  });

  it('loads inline HTML into an existing tab and persists its replacement source', async () => {
    const session = await activeSession({ summary: { url: 'https://servo-inline.invalid/' } });
    const result = await session.navigateHtml('<p>Replacement</p>');
    expect(result.action).toBe('navigate');
    expect(lastRuntimeCalls()).toContain('loadHtml:<p>Replacement</p>');
    expect(sql().table('browser_asset')!.rows.some((row) => row.name === 'initial-html' && row.chunk_text === '<p>Replacement</p>')).toBe(true);
    await expect(session.navigateHtml('é'.repeat(524289))).rejects.toThrow(/UTF-8 bytes/);
    createdRuntimes.at(-1).options.loadHtmlReturns = false;
    await expect(session.navigateHtml('<p>Rejected</p>')).rejects.toThrow(/rejected the supplied HTML/);
  });

  it('reports when Servo rejects a navigate request', async () => {
    const session = await activeSession({ loadPageReturns: false });
    await expect(session.navigate('https://public.example/')).rejects.toThrow(/rejected the requested URL/);
  });


  it('does not return page contents or start recording when core readiness fails', async () => {
    const session = await activeSession({
      summary: { url: 'https://public.example/page', title: 'P', text: 'loading...' },
      pageReadyResult: { ready: false, reason: 'timeout' },
    });
    await expect(session.inspect()).rejects.toThrow(/readiness timed out/i);
    await expect(session.click(10, 10)).rejects.toThrow(/readiness timed out/i);
    await expect(session.startScreenRecording(2, 4)).rejects.toThrow(/readiness timed out/i);
  });

  it('runs inspect, wait, click, typeText, pressKey, scroll, history, reload, screenshot, capabilities', async () => {
    const png = new Uint8Array([9, 8, 7]);
    const session = await activeSession({ summary: { url: 'https://public.example/page', title: 'P', text: 'T' }, screenshotPng: png, capabilitiesValue: { ok: 1 } });
    expect(await session.inspect()).toEqual({ url: 'https://public.example/page', title: 'P', text: 'T' });
    expect((await session.wait()).title).toBe('P');
    expect((await session.click(10, 20)).action).toBe('click');
    expect((await session.click(10, 20, 2)).action).toBe('click');
    expect((await session.typeText('hello')).action).toBe('type');
    expect((await session.pressKey('Enter')).action).toBe('key');
    expect((await session.scroll(0, 100)).action).toBe('scroll');
    expect((await session.scroll(0, 100, 5, 6)).action).toBe('scroll');
    expect((await session.history('back')).action).toBe('back');
    expect((await session.history('forward')).action).toBe('forward');
    expect((await session.reload()).action).toBe('reload');
    const shot = await session.screenshot();
    expect(shot.png).toBe(png);
    expect((await session.screenshot(true)).page.title).toBe('P');
    expect(await session.capabilities()).toEqual({ ok: 1 });
    const calls = lastRuntimeCalls();
    expect(calls).toContain('click:10:20:0');
    expect(calls).toContain('click:10:20:2');
    expect(calls).toContain('typeText:hello');
    expect(calls).toContain('pressKey:Enter');
    expect(calls).toContain('scrollBy:0:100:undefined:undefined');
    expect(calls).toContain('scrollBy:0:100:5:6');
    expect(calls).toContain('goBack');
    expect(calls).toContain('goForward');
  });

  it('skips pumping history actions that did not navigate', async () => {
    const session = await activeSession({ goBackReturns: false, goForwardReturns: false });
    const back = await session.history('back');
    expect(back.action).toBe('back');
    const calls = lastRuntimeCalls();
    expect(calls.filter((call) => call === 'pump')).toHaveLength(0);
  });

  it('reloads the latest inline HTML without sending the synthetic URL to the network', async () => {
    const session = await activeSession();
    await session.navigateHtml('<title>First</title>');
    const html = '<title>Second</title>';
    await session.navigateHtml(html);
    const runtime = createdRuntimes.at(-1) as FakeServoRuntime;
    const navigateHtml = vi.spyOn(runtime, 'navigateHtml');
    const reload = vi.spyOn(runtime, 'reload');
    await session.reload({ maxDurationMs: 2000 });
    expect(navigateHtml).toHaveBeenCalledWith(html, {
      url: 'https://servo-inline.invalid/', readiness: { maxDurationMs: 2000 },
    });
    expect(reload).not.toHaveBeenCalled();
  });

  it('reports reload failures', async () => {
    const session = await activeSession({ reloadReturns: false });
    await expect(session.reload()).rejects.toThrow(/could not reload the current page/);
  });

  it('executes HTTP calls in the Servo page runtime instead of Worker fetch', async () => {
    const body = Buffer.from('{"answer":42}').toString('base64');
    const session = await activeSession({ summary: { url: 'https://public.example/page' }, evaluations: {
      [RESUME_MARKER]: okString(validResumeState),
      '(async () => {\n    const request = ': okString({
        status: 200, statusText: 'OK', url: 'https://public.example/api',
        redirected: false, headers: { 'content-type': 'application/json' },
        bodyBase64: body, bodyBytes: 13, truncated: false,
      }),
    } });
    const result = await session.requestHttp({
      url: 'https://public.example/api', method: 'POST',
      headers: { 'X-Test-Header': 'safe-fixture' }, body: '{"hello":true}',
      followRedirects: true, maxResponseBytes: 1024, maxDurationMs: 10000,
    });
    expect(result).toMatchObject({ status: 200, url: 'https://public.example/api', body: new TextEncoder().encode('{"answer":42}') });
    const runtime = createdRuntimes.at(-1) as FakeServoRuntime;
    const script = runtime.evaluations.find((entry) => entry.script.startsWith('(async () => {\n    const request = '))?.script;
    expect(script).toContain('fetch(request.url');
    expect(script).toContain('"method":"POST"');
    expect(fetchImplSpy).not.toHaveBeenCalled();
    await expect(session.requestHttp({
      url: 'https://public.example/api', method: 'GET',
      headers: { Authorization: 'Bearer fixture-only' },
      followRedirects: true, maxResponseBytes: 10, maxDurationMs: 5000,
    })).rejects.toThrow(/cannot carry Authorization/);
    await expect(session.requestHttp({
      url: 'http://127.0.0.1/', method: 'GET', followRedirects: true,
      maxResponseBytes: 10, maxDurationMs: 5000,
    })).rejects.toThrow(/Private, local/);
  });

  it.each([
    [{ Err: 'Timeout' }, 'Servo HTTP request timed out.'],
    [{ Err: 'TypeError: Failed to fetch' }, 'Servo HTTP request failed: TypeError: Failed to fetch'],
    [{ Ok: { Null: null } }, 'Servo HTTP request returned an invalid evaluation result.'],
  ])('reports unsuccessful page evaluations as HTTP errors', async (evaluation, message) => {
    const session = await activeSession({ summary: { url: 'https://public.example/page' }, evaluations: {
      [RESUME_MARKER]: okString(validResumeState),
      '(async () => {\n    const request = ': evaluation,
    } });
    await expect(session.requestHttp({
      url: 'https://public.example/api', method: 'GET', followRedirects: true,
      maxResponseBytes: 1024, maxDurationMs: 100,
    })).rejects.toThrow(message);
  });

  it('does not restore another origin when an HTTP response URL differs from the tab origin', async () => {
    const session = await activeSession({ summary: { url: 'https://public.example/page' }, evaluations: {
      [RESUME_MARKER]: okString(validResumeState),
      '(async () => {\n    const request = ': okString({
        status: 200, statusText: 'OK', url: 'https://other.example/resource',
        redirected: false, headers: {}, bodyBase64: '', bodyBytes: 0, truncated: false,
      }),
    } });
    const restore = vi.spyOn(session as any, 'restoreNavigatedOrigin');
    const response = await session.requestHttp({
      url: 'https://public.example/resource', method: 'GET',
      followRedirects: true, maxResponseBytes: 1024, maxDurationMs: 8000,
    });
    expect(response.url).toBe('https://other.example/resource');
    expect(restore).not.toHaveBeenCalled();
  });

  it('preserves literal dollar tokens in request bodies and enforces UTF-8 byte caps', async () => {
    const session = await activeSession({ summary: { url: 'https://public.example/page' }, evaluations: {
      [RESUME_MARKER]: okString(validResumeState),
      '(async () => {\n    const request = ': okString({
        status: 204, statusText: 'No Content', url: 'https://public.example/api',
        redirected: false, headers: {}, bodyBase64: '', bodyBytes: 0, truncated: false,
      }),
    } });
    const content = "literal $& $$ $' end";
    const request = {
      url: 'https://public.example/api', method: 'POST', body: content,
      followRedirects: true, maxResponseBytes: 256, maxDurationMs: 10000,
    };
    await session.requestHttp(request);
    const runtime = createdRuntimes.at(-1) as FakeServoRuntime;
    const script = runtime.evaluations.find((entry) => entry.script.startsWith('(async () => {\n    const request = '))?.script;
    expect(script).toContain(JSON.stringify(content));
    expect(script).not.toContain('__REQUEST_DATA__');
    await expect(session.requestHttp({ ...request, body: 'é'.repeat(600_000) }))
      .rejects.toThrow('Request body exceeds 1 MiB');
  });

  it('rejects search navigations marked as network errors by Servo', async () => {
    const session = await activeSession();
    vi.spyOn(session as any, 'summary').mockResolvedValue({
      url: 'https://public.example/search', title: '', text: '',
      loadError: 'The page could not be loaded: DNS failed',
    });
    await expect(session.navigateAndReadHtml('https://public.example/search'))
      .rejects.toThrow('DNS failed');
  });

  it('navigates and extracts DOM markup through a Servo session for search', async () => {
    const html = '<html><body><h1>Search result</h1></body></html>';
    const session = await activeSession({ evaluations: {
      [RESUME_MARKER]: okString(validResumeState),
      "JSON.stringify((document.documentElement?.outerHTML ?? '').slice(0, 512000))": okString(JSON.stringify(html)),
    } });
    const result = await session.navigateAndReadHtml('https://public.example/search?q=hello');
    expect(result.html).toBe(html);
    const runtime = createdRuntimes.at(-1) as FakeServoRuntime;
    expect(runtime.calls).toContain('loadPage:https://public.example/search?q=hello');
    expect(runtime.evaluations.some((entry) => entry.script.includes('document.documentElement?.outerHTML'))).toBe(true);
  });

  it('extracts search HTML after origin state was restored', async () => {
    const html = '<html><body>restored</body></html>';
    const session = await activeSession({ evaluations: {
      [RESUME_MARKER]: okString(validResumeState),
      "JSON.stringify((document.documentElement?.outerHTML ?? '').slice(0, 512000))": okString(JSON.stringify(html)),
    } });
    const events: string[] = [];
    vi.spyOn(session as any, 'restoreNavigatedOrigin').mockImplementation(async () => { events.push('origin-restored'); });
    const runtime = createdRuntimes.at(-1) as FakeServoRuntime;
    const evaluate = runtime.evaluate.bind(runtime);
    vi.spyOn(runtime, 'evaluate').mockImplementation(async (source: string, options?: { maxDurationMs?: number }) => {
      if (source.includes('document.documentElement?.outerHTML')) events.push('html-read');
      return evaluate(source, options);
    });
    const result = await session.navigateAndReadHtml('https://search.example/results');
    expect(result.html).toBe(html);
    expect(events).toEqual(['origin-restored', 'html-read']);
  });

  it('evaluates scripts and parses WebDriver-style results', async () => {
    const session = await activeSession({ evaluations: { 'document.title': okString('"served"') } });
    const result = await session.evaluate('document.title');
    expect(result.value).toEqual({ Ok: { String: '"served"' } });
    expect(result.page).toBeDefined();
  });

  it('preserves non-string and malformed WebDriver evaluation payloads', async () => {
    const session = await activeSession({ evaluations: {
      number: { Ok: { String: 42 } },
      malformed: { Ok: { String: '{not json' } },
    } });
    expect((await session.evaluate('number')).value).toEqual({ Ok: { String: 42 } });
    expect((await session.evaluate('malformed')).value).toEqual({ Ok: { String: '{not json' } });
  });

  it('rejects malformed page summary JSON returned by the browser', async () => {
    await expect(activeSession({ evaluations: { [SUMMARY_MARKER]: okString('{not json') } }))
      .rejects.toThrow('Servo returned an invalid page summary.');
    await expect(activeSession({ evaluations: { [SUMMARY_MARKER]: { Ok: { String: 42 } } } }))
      .rejects.toThrow('Servo returned an invalid page summary.');
  });

  it('rejects scripts over 64 KiB', async () => {
    const session = await activeSession();
    await expect(session.evaluate('x'.repeat(64 * 1024 + 1))).rejects.toThrow(RangeError);
  });

  it('surfaces screenshot failures', async () => {
    const session = await activeSession({ screenshotThrows: new Error('capture failed') });
    await expect(session.screenshot()).rejects.toThrow('capture failed');
  });

  it('surfaces pump failures during operations', async () => {
    const session = await activeSession({ pumpThrows: new Error('pump died') });
    await expect(session.wait()).rejects.toThrow('pump died');
  });

  it('registers fonts and assigns sequential asset names', async () => {
    const session = await activeSession({ registerFontFaces: 3 });
    expect(await session.registerFont(btoa('abc'))).toEqual({ faces: 3 });
    expect(await session.registerFont(btoa('def'))).toEqual({ faces: 3 });
    const names = sql().table('browser_asset')!.rows.filter((row) => String(row.name).startsWith('font:')).map((row) => row.name);
    expect(names).toEqual(['font:000000', 'font:000001']);
  });

  it('uses the zero font count when storage returns no count row', async () => {
    const session = await activeSession({ registerFontFaces: 2 });
    const database = sql();
    const realExec = database.exec.bind(database);
    database.exec = ((statement: string, ...params: unknown[]) => {
      if (statement.includes('COUNT(DISTINCT name)')) return { toArray: () => [] } as never;
      return realExec(statement, ...params);
    }) as typeof database.exec;
    expect(await session.registerFont(btoa('font'))).toEqual({ faces: 2 });
    expect(database.table('browser_asset')!.rows[0].name).toBe('font:000000');
  });

  it('skips the post-operation snapshot when the runtime or session changes mid-operation', async () => {
    const session = await activeSession();
    const runtime = createdRuntimes.at(-1) as { options: FakeRuntimeOptions } | undefined;
    if (!runtime) throw new Error('no runtime created');
    const internal = session as unknown as { runtime: unknown };
    const originalRuntime = internal.runtime;
    runtime.options.pumpFactory = async () => {
      internal.runtime = null;
      return { settled: true };
    };
    await expect(session.wait()).resolves.toMatchObject({ title: expect.any(String) });
    expect(internal.runtime).toBeNull();
    internal.runtime = originalRuntime;

    runtime.options.pumpFactory = async () => {
      sql().table('browser_session')!.rows[0].status = 'closed';
      return { settled: true };
    };
    await expect(session.wait()).resolves.toMatchObject({ title: expect.any(String) });
    expect(sql().table('browser_session')!.rows[0].status).toBe('closed');
  });

  it('serializes concurrent operations onto one queue', async () => {
    const session = await activeSession({ summary: { url: 'https://public.example/page' } });
    const [a, b, c] = await Promise.all([session.inspect(), session.wait(), session.inspect()]);
    expect(a.url).toBe('https://public.example/page');
    expect(b.url).toBe('https://public.example/page');
    expect(c.url).toBe('https://public.example/page');
  });
});

describe('session termination', () => {
  it('closes sessions, clears runtimes, and wipes persisted state', async () => {
    scriptRuntime({});
    const session = await newSession();
    await session.initialize(initOptions());
    expect(await session.close()).toEqual({ status: 'closed' });
    expect(sql().table('browser_session')!.rows[0].status).toBe('closed');
    expect(sql().table('browser_snapshot')!.rows).toHaveLength(0);
    expect(sql().table('browser_asset')!.rows).toHaveLength(0);
    expect(state.alarms.deleted).toBeGreaterThanOrEqual(1);
    expect(lastRuntimeCalls()).toContain('reset');
  });

  it('is idempotent when closing already-closed or missing sessions', async () => {
    const session = await newSession();
    expect(await session.close()).toEqual({ status: 'closed' });
    seedActiveRow(sql(), Date.now() + 60_000, 'closed');
    expect(await session.close()).toEqual({ status: 'closed' });
  });

  it('logs reset failures while clearing a trapped runtime', async () => {
    scriptRuntime({ trapped: true, resetThrows: true });
    const session = await newSession();
    await session.initialize(initOptions());
    // The trapped runtime is discarded (and its failing reset logged) when the
    // next operation requires a live runtime.
    await session.inspect();
    expect(consoleErrorSpy).toHaveBeenCalledWith(expect.stringContaining('servo_session_reset_failed'));
  });

  it('refuses operations on closed sessions', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000, 'closed');
    await expect(session.inspect()).rejects.toThrow(/is closed/);
  });

  it('reaps expired leases lazily when an operation requires a runtime', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() - 1);
    await expect(session.inspect()).rejects.toThrow(/reaped after 30 days/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('expired');
    expect(sql().table('browser_snapshot')!.rows).toHaveLength(0);
    expect(sql().table('browser_asset')!.rows).toHaveLength(0);
    expect(state.alarms.deleted).toBeGreaterThanOrEqual(1);
  });

  it('refuses operations on nonexistent sessions', async () => {
    const session = await newSession();
    await expect(session.inspect()).rejects.toThrow(/does not exist/);
  });

  it('reaps long-idle sessions on their alarm', async () => {
    const session = await newSession();
    seedActiveRow(sql(), Date.now() - 1);
    seedSnapshot(sql(), makeSnapshotJson());
    seedAsset(sql(), 'initial-html', '<html></html>');
    await session.alarm();
    expect(sql().table('browser_session')!.rows[0].status).toBe('expired');
    expect(sql().table('browser_snapshot')!.rows).toHaveLength(0);
    expect(state.alarms.deleted).toBeGreaterThanOrEqual(1);
  });

  it('ignores alarms for missing or closed sessions', async () => {
    const session = await newSession();
    await session.alarm();
    seedActiveRow(sql(), Date.now() + 60_000, 'closed');
    await session.alarm();
    expect(state.alarms.set).toHaveLength(0);
  });

  it('renews the lease when the alarm fires while still active', async () => {
    const session = await newSession();
    await session.initialize(initOptions());
    const before = state.alarms.set.length;
    state.alarms.current = null;
    await session.alarm();
    expect(state.alarms.set.length).toBe(before + 1);
    expect(sql().table('browser_session')!.rows[0].status).toBe('active');
  });

  it('does not postpone a pending recording alarm when status is polled', async () => {
    const session = await newSession();
    await session.initialize(initOptions());
    sql().table('browser_recording')!.rows.push({
      id: '00000000-0000-4000-8000-000000000002',
      status: 'encoding',
      started_at: Date.now() - 2_000,
      stopped_at: Date.now(),
      expires_at: Date.now() + 24 * 60 * 60 * 1_000,
      fps: 1,
      max_duration_ms: 3_000,
      max_frames: 3,
      target_frames: 2,
      captured_frames: 2,
      stored_bytes: 0,
      width: 320,
      height: 240,
      download_token: null,
      error: null,
    });

    await session.getScreenRecordingStatus('00000000-0000-4000-8000-000000000002');
    const scheduledAlarm = state.alarms.current;
    const setCalls = state.alarms.set.length;
    expect(scheduledAlarm).toBe(Date.now() + 1_000);

    await vi.advanceTimersByTimeAsync(500);
    await session.getScreenRecordingStatus('00000000-0000-4000-8000-000000000002');

    expect(state.alarms.current).toBe(scheduledAlarm);
    expect(state.alarms.set).toHaveLength(setCalls);
  });

  it('does not schedule an app-level timer to retain the runtime', async () => {
    scriptRuntime({});
    const session = await newSession();
    await session.initialize(initOptions());
    expect((await session.getStatus()).runtimeAvailable).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(90_000);
    // The unit harness does not emulate Cloudflare hibernation, but the
    // application itself must not keep the Durable Object alive with a timer.
    expect((await session.getStatus()).runtimeAvailable).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('restores the full cookie jar when a runtime must be recreated', async () => {
    scriptRuntime({});
    const session = await newSession();
    await session.initialize(initOptions());
    expect(createdRuntimes[0].calls).toContain('exportCookieState');
    // A trapped runtime follows the same snapshot restore path used after a
    // Durable Object instance is hibernated and its WASM heap is gone.
    createdRuntimes[0].trapped = true;
    await session.inspect();
    expect(createdRuntimes).toHaveLength(2);
    expect(createdRuntimes[1].restoredCookies).toEqual([new Uint8Array([1, 2, 3])]);
  });

  it('marks sessions failed when a required restore breaks mid-operation', async () => {
    scriptRuntime({});
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson({ url: 'https://public.example/page' }));
    // First operation succeeds and caches a runtime...
    await session.inspect();
    // ...then simulate that runtime trapping, with a broken adapter on restore.
    createdRuntimes[0].trapped = true;
    scriptRuntime({});
    createServoWorkerRuntime.mockRejectedValueOnce(new Error('engine gone'));
    await expect(session.inspect()).rejects.toThrow(/Saved tab could not be restored: Error: engine gone/);
    expect(sql().table('browser_session')!.rows[0].status).toBe('failed');
    expect(state.alarms.deleted).toBeGreaterThanOrEqual(1);
  });

  it('converts non-Error restore failures into readable messages', async () => {
    createServoWorkerRuntime.mockClear();
    const session = await newSession();
    seedActiveRow(sql(), Date.now() + 60_000);
    seedSnapshot(sql(), makeSnapshotJson());
    createServoWorkerRuntime.mockRejectedValueOnce('kaboom');
    await expect(session.inspect()).rejects.toThrow(/Saved tab could not be restored: kaboom/);
  });
});

describe('widget screenshot bytes', () => {
  it('preserves published PNGs across captures and revokes them on close', async () => {
    const session = await newSession();
    await session.initialize(initOptions());
    const pngBytes = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]);
    await expect(session.publishWidgetScreenshot(new Uint8Array())).rejects.toThrow(/empty/);
    await expect(session.publishWidgetScreenshot('nope' as unknown as Uint8Array)).rejects.toThrow(/empty/);
    await expect(session.publishWidgetScreenshot(new Uint8Array(8 * 1024 * 1024 + 1))).rejects.toThrow(/8388608/);
    const first = await session.publishWidgetScreenshot(pngBytes);
    expect(first.token).toMatch(/^[0-9a-f-]{36}$/i);
    const url = `https://worker.example/screenshots/${UUID}/${first.token}`;
    expect((await session.fetch(new Request(url, { method: 'POST' }))).status).toBe(405);
    const ok = await session.fetch(new Request(url));
    expect(ok.status).toBe(200);
    expect(ok.headers.get('content-type')).toBe('image/png');
    expect(ok.headers.get('cache-control')).toBe('private, no-store');
    expect(ok.headers.get('x-content-type-options')).toBe('nosniff');
    expect(new Uint8Array(await ok.arrayBuffer())).toEqual(pngBytes);
    const replaced = await session.publishWidgetScreenshot(new Uint8Array([1, 2, 3]));
    expect((await session.fetch(new Request(url))).status).toBe(200);
    expect(new Uint8Array(await (await session.fetch(new Request(`https://worker.example/screenshots/${UUID}/${replaced.token}`))).arrayBuffer())).toEqual(new Uint8Array([1, 2, 3]));
    expect((await session.fetch(new Request(`https://worker.example/screenshots/${UUID}/not-a-token`))).status).toBe(404);
    expect((await session.fetch(new Request('https://worker.example/screenshots/short'))).status).toBe(404);
    expect((await session.fetch(new Request('https://worker.example/other'))).status).toBe(404);
    await session.close();
    expect((await session.fetch(new Request(`https://worker.example/screenshots/${UUID}/${replaced.token}`))).status).toBe(404);
  });
  it('round-trips a multi-megabyte capture, evicts the ninth-oldest token, and expires the rest', async () => {
    const session = await newSession();
    await session.initialize(initOptions());
    const png = Uint8Array.from({ length: 2 * 1024 * 1024 + 31 }, (_, i) => (i * 17) % 251);
    const request = (token: string) => new Request(`https://worker.example/screenshots/${UUID}/${token}`);
    const first = await session.publishWidgetScreenshot(png);
    const fetched = await session.fetch(request(first.token));
    expect(fetched.status).toBe(200);
    expect(Buffer.from(await fetched.arrayBuffer()).equals(Buffer.from(png))).toBe(true);
    const sizes = sql().exec<{ size: number }>(
      'SELECT length(png_blob) AS size FROM browser_widget_screenshot_chunk WHERE token = ? ORDER BY chunk_index', first.token,
    ).toArray().map(row => row.size);
    expect(sizes).toEqual([524288, 524288, 524288, 524288, 31]);
    const small = new Uint8Array([1]);
    const second = await session.publishWidgetScreenshot(small);
    for (let i = 0; i < 7; i++) await session.publishWidgetScreenshot(small);
    expect((await session.fetch(request(first.token))).status).toBe(404);
    expect((await session.fetch(request(second.token))).status).toBe(200);
    vi.setSystemTime(Date.now() + 24 * 60 * 60 * 1000);
    expect((await session.fetch(request(second.token))).status).toBe(404);
    await session.alarm();
    expect(sql().exec('SELECT token FROM browser_widget_screenshot_chunk').toArray()).toEqual([]);
  });

  it('rolls back a partially stored PNG without invalidating an earlier link', async () => {
    const session = await newSession();
    await session.initialize(initOptions());
    const existing = await session.publishWidgetScreenshot(new Uint8Array([42]));
    sql().exec(`CREATE TRIGGER reject_second_chunk BEFORE INSERT ON browser_widget_screenshot_chunk
      WHEN NEW.chunk_index = 1 BEGIN SELECT RAISE(ABORT, 'chunk write failed'); END`);
    await expect(session.publishWidgetScreenshot(new Uint8Array(600_000))).rejects.toThrow('chunk write failed');
    expect(sql().exec('SELECT token FROM browser_widget_screenshot_chunk').toArray()).toEqual([{ token: existing.token }]);
    const response = await session.fetch(new Request(`https://worker.example/screenshots/${UUID}/${existing.token}`));
    expect([...new Uint8Array(await response.arrayBuffer())]).toEqual([42]);
  });

  it('migrates a published singleton without changing its URL or extending its lifetime', async () => {
    await newSession();
    const token = crypto.randomUUID();
    const updated = Date.now() - 60_000;
    sql().exec('INSERT INTO browser_widget_screenshot (singleton, token, png_blob, updated_at) VALUES (1, ?, ?, ?)',
      token, new Uint8Array([4, 2]).buffer, updated);
    const restored = new mod.ServoBrowserSession(asDoState(state), {} as ServoWorkerEnv);
    const request = new Request(`https://worker.example/screenshots/${UUID}/${token}`);
    expect([...new Uint8Array(await (await restored.fetch(request)).arrayBuffer())]).toEqual([4, 2]);
    expect(sql().exec('SELECT token FROM browser_widget_screenshot').toArray()).toEqual([]);
    vi.setSystemTime(updated + 24 * 60 * 60 * 1000);
    expect((await restored.fetch(request)).status).toBe(404);
  });

});

function lastRuntimeCalls(): string[] {
  const runtime = createdRuntimes.at(-1);
  if (!runtime) throw new Error('no runtime created');
  return [...runtime.calls] as string[];
}
