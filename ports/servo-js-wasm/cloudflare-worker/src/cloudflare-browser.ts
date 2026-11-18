import { createServoWorkerRuntime } from './vendor/servo-worker/worker-adapter.mjs';
import { applyChromeRequestDefaults } from './chrome-request-profile';

/**
 * Cloudflare-specific Servo browser factory.
 *
 * Provides MESH-backed egress (cf1:network) for all browser network activity
 * without requiring the vendor adapter to export the higher-level helper.
 * Preserves the required public-network policy and onNetworkResult callback.
 */
export async function createCloudflareServoBrowser(
  wasmModule: WebAssembly.Module,
  env: { MESH?: { fetch: typeof fetch } },
  options: Record<string, unknown> & { onNetworkResult?: (url: string, status: number | null, error: unknown) => void } = {},
) {
  const { onNetworkResult, ...browserOptions } = options;
  const transport = createCloudflareTransport(env, onNetworkResult);
  return createServoWorkerRuntime(wasmModule, { ...browserOptions, onNetworkResult, ...transport });
}

function createCloudflareTransport(
  env: { MESH?: { fetch: typeof fetch } },
  onNetworkResult: ((url: string, status: number | null, error: unknown) => void) | undefined = () => {},
) {
  if (!env?.MESH || typeof env.MESH.fetch !== 'function') {
    throw new Error('Servo-WASM requires a Cloudflare MESH binding for browser egress.');
  }
  const mesh = env.MESH;
  const ensureAllowed = (value: string, socket = false) => {
    const url = new URL(value);
    if (!(socket ? ['ws:', 'wss:'].includes(url.protocol) : ['http:', 'https:'].includes(url.protocol))) {
      throw new TypeError('Unsupported browser network URL protocol');
    }
    if (url.username || url.password) throw new TypeError('Credentials in URLs are prohibited');
    const h = url.hostname.toLowerCase().replace(/^[\[]|[\]]$/g, '');
    if (h === 'localhost' || /\.(?:localhost|local|internal|test|invalid)$/.test(h) ||
        h === 'metadata.google.internal' || h === '::' || h === '::1' ||
        /^::ffff:/i.test(h) || /^[fc][0-9a-f]/i.test(h) || /^fe[89ab]/i.test(h) ||
        /^(?:0|10|127|169\.254|192\.168|224|240)\./.test(h) ||
        /^172\.(?:1[6-9]|2\d|3[01])\./.test(h) ||
        /^100\.(?:6[4-9]|[78]\d|9\d|1[01]\d|12[0-7])\./.test(h) ||
        /^198\.(?:18|19)\./.test(h) || /^192\.0\.0\./.test(h)) {
      throw new TypeError('Private, local or metadata network destination blocked');
    }
    return url;
  };
  const fetchImpl = async (
    input: RequestInfo | URL,
    init?: RequestInit,
    requestContext?: { destination?: unknown; preflight?: boolean },
  ) => {
    const url = input instanceof Request ? input.url : String(input);
    ensureAllowed(url);
    try {
      const request = new Request(input, { ...init, redirect: 'manual' });
      const response = await mesh.fetch(applyChromeRequestDefaults(request, requestContext));
      onNetworkResult?.(url, response.status, null);
      return response;
    } catch (error) {
      onNetworkResult?.(url, null, error);
      throw error;
    }
  };
  const webSocketFactory = (url: string, protocols?: string | string[]) => {
    const verified = ensureAllowed(url, true);
    const pair = new WebSocketPair();
    const [client, bridge] = Object.values(pair);
    client.accept();
    bridge.accept();
    const closeBoth = (code = 1011, reason = 'Mesh WebSocket bridge closed') => {
      for (const socket of [client, bridge]) {
        try { socket.close(code, reason.slice(0, 100)); } catch {}
      }
    };
    void (async () => {
      const target = verified.href.replace(/^ws:/, 'http:').replace(/^wss:/, 'https:');
      const headers = new Headers({ Upgrade: 'websocket' });
      if (protocols) headers.set('Sec-WebSocket-Protocol', Array.isArray(protocols) ? protocols.join(', ') : protocols);
      const response = await mesh.fetch(target, { headers });
      const remote = response.webSocket;
      if (response.status !== 101 || !remote) throw new Error('Mesh refused WebSocket upgrade');
      remote.accept();
      bridge.addEventListener('message', event => { try { remote.send(event.data); } catch { closeBoth(); } });
      remote.addEventListener('message', event => { try { bridge.send(event.data); } catch { closeBoth(); } });
      bridge.addEventListener('close', event => { try { remote.close(event.code || 1000, event.reason); } catch {} });
      remote.addEventListener('close', event => closeBoth(event.code || 1000, event.reason));
      remote.addEventListener('error', () => closeBoth());
    })().catch(error => { onNetworkResult?.(verified.href, null, error); closeBoth(); });
    return client;
  };
  return { fetchImpl, webSocketFactory };
}
