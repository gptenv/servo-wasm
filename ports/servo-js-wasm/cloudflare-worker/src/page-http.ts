import { z } from 'zod';
import type { createServoWorkerRuntime } from './vendor/servo-worker/worker-adapter.mjs';
import { httpRequestSchema, MAX_HTTP_REQUEST_BYTES, type HttpRequest, type ServoHttpResponse } from './http-contract';
import { assertPublicHttpUrl } from './security';

const responseSchema = z.object({
  status: z.number().int().min(0).max(599),
  statusText: z.string(),
  headers: z.record(z.string(), z.string()),
  url: z.string(),
  bodyBase64: z.string(),
  bodyBytes: z.number().int().nonnegative(),
  truncated: z.boolean(),
  redirected: z.boolean(),
});

export async function requestPageHttp(
  runtime: Awaited<ReturnType<typeof createServoWorkerRuntime>>,
  input: HttpRequest,
): Promise<ServoHttpResponse> {
  const url = assertPublicHttpUrl(input.url);
  const parsedInput = httpRequestSchema.parse(input);
  const { headers, body, followRedirects, maxDurationMs, maxResponseBytes } = parsedInput;
  const method = parsedInput.method.toUpperCase();
  for (const name of Object.keys(headers ?? {})) {
    if (/^(authorization|proxy-authorization|cookie|cookie2|x-api-key|x-auth-token|x-access-token)$/i.test(name.trim())) {
      throw new TypeError('Servo HTTP requests cannot carry ' + name + ' headers.');
    }
  }
  if (/^(?:CONNECT|TRACE|TRACK)$/.test(method)) throw new TypeError(method + ' is forbidden by the Fetch API.');
  if (body !== undefined && (method === 'GET' || method === 'HEAD')) throw new TypeError(method + ' cannot carry a body');
  if (body !== undefined && new TextEncoder().encode(body).byteLength > MAX_HTTP_REQUEST_BYTES) {
    throw new RangeError('Request body exceeds 1 MiB of UTF-8.');
  }
  const current = await runtime.evaluate('location.href', { maxDurationMs });
  const pageUrl = typeof current?.Ok?.String === 'string' ? current.Ok.String : null;
  if (pageUrl === null || new URL(pageUrl).origin !== url.origin) {
    throw new Error('Servo HTTP origin mismatch. Navigate to the destination origin first.');
  }
  const requestPayload = JSON.stringify({
    url: url.href,
    method,
    headers: headers ?? {},
    body,
    followRedirects,
    maxResponseBytes,
    maxDurationMs,
  });
  const script = `(async () => {
    const request = ${requestPayload};
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), request.maxDurationMs);
    try {
      const response = await fetch(request.url, {
        method: request.method,
        headers: request.headers,
        ...(request.body === undefined ? {} : { body: request.body }),
        redirect: request.followRedirects ? 'follow' : 'manual',
        signal: controller.signal,
      });
      const headers = Object.fromEntries(response.headers.entries());
      let bodyBase64 = '';
      let bodyBytes = 0;
      let truncated = false;
      if (request.method !== 'HEAD' && response.body) {
        const reader = response.body.getReader();
        const chunks = [];
        const max = request.maxResponseBytes;
        try {
        while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            if (bodyBytes + value.byteLength > max) {
              const remaining = max - bodyBytes;
              if (remaining > 0) chunks.push(value.slice(0, remaining));
              bodyBytes += Math.max(remaining, 0);
              truncated = true;
              await reader.cancel();
              break;
            }
            chunks.push(value);
            bodyBytes += value.byteLength;
          }

      } finally {
        reader.releaseLock();
      }
      const bytes = new Uint8Array(bodyBytes);
        let offset = 0;
        for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
        // Encode in bounded chunks to avoid argument/string size limits.
        // Keep every non-final chunk on a 3-byte boundary so the base64
        // padding from intermediate chunks cannot corrupt the final value.
        for (let start = 0; start < bytes.length; start += 0x7ffe) {
          bodyBase64 += btoa(String.fromCharCode(...bytes.subarray(start, start + 0x7ffe)));
        }
      }
      return JSON.stringify({
        status: response.status,
        statusText: response.statusText,
        headers,
        url: response.url,
        bodyBase64,
        bodyBytes,
        truncated,
        redirected: response.redirected,
      });
    } finally {
      clearTimeout(timer);
    }
  })()`;
  // The browser's abort timer gets a chance to report its error before the host
  // stops pumping the evaluation. A wedged page is still bounded by the host.
  const result = await runtime.evaluate(script, { maxDurationMs: maxDurationMs + 250 });
  if (result && typeof result === 'object' && 'Err' in result) {
    const detail = typeof result.Err === 'string' ? result.Err : JSON.stringify(result.Err) ?? 'unknown error';
    if (/timeout|abort/i.test(detail)) throw new Error('Servo HTTP request timed out.');
    throw new Error('Servo HTTP request failed: ' + detail.slice(0, 512));
  }
  if (typeof result?.Ok?.String !== 'string') {
    throw new Error('Servo HTTP request returned an invalid evaluation result.');
  }
  const { bodyBase64, ...response } = responseSchema.parse(JSON.parse(result.Ok.String));
  // Verify the payload before it crosses the Worker boundary and decode it only once.
  let binary: string;
  try {
    binary = atob(bodyBase64);
  } catch {
    throw new Error('Servo HTTP response contained invalid body encoding.');
  }
  if (binary.length !== response.bodyBytes || binary.length > maxResponseBytes) {
    throw new Error('Servo HTTP response contained an invalid body length.');
  }
  return { ...response, body: Uint8Array.from(binary, (char) => char.charCodeAt(0)) };
}
