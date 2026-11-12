import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

// Evaluate only the transport closure to avoid booting the WebAssembly engine.
// This exercises the exact production validation code, not a copied validator.
const source = readFileSync(new URL('../worker-adapter.mjs', import.meta.url), 'utf8');
const from = source.indexOf('function createCloudflareTransport(');
const returnIdx = source.indexOf('return { fetchImpl, webSocketFactory };', from);
const to = source.indexOf('\n}', returnIdx) + 2;
assert.ok(from >= 0 && returnIdx > from && to > returnIdx, 'Cloudflare transport implementation must be present');
const makeTransport = new Function(source.slice(from, to) + '\nreturn createCloudflareTransport;')();

test('Cloudflare browser egress blocks private/local and metadata destinations before calling Mesh', async () => {
  const calls = [];
  const transport = makeTransport({ MESH: { fetch: async (req) => {
    calls.push(req);
    return new Response('ok');
  } } });
  for (const url of [
    'http://127.0.0.1/', 'http://10.1.2.3/', 'http://172.16.0.1/',
    'http://192.168.1.1/', 'http://169.254.169.254/',
    'http://100.100.100.100/', 'http://198.18.0.1/',
    'http://localhost/', 'https://service.internal/', 'https://metadata.google.internal/',
    'http://[::1]/', 'http://[fc00::1]/', 'http://[fe80::1]/',
  ]) {
    await assert.rejects(() => transport.fetchImpl(url), /blocked/);
  }
  assert.equal(calls.length, 0, 'Rejected targets must never reach Mesh');
  await transport.fetchImpl('https://example.org/');
  assert.equal(calls.length, 1);
  assert.equal(calls[0].redirect, 'manual');
});
