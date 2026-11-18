import { describe, expect, it } from 'vitest';
import { assertPublicHttpUrl, assertPublicWebSocketUrl } from '../src/security';

describe('public network URL policy', () => {
  it.each(['https://example.com/', 'http://8.8.8.8/'])('allows public HTTP(S) URL %s', (url) => {
    expect(assertPublicHttpUrl(url).href).toBe(url);
  });

  it.each([
    'file:///etc/passwd',
    'ftp://example.com/',
    'http://localhost/',
    'http://service.local/',
    'http://metadata.google.internal/',
    'http://127.0.0.1/',
    'http://10.0.0.1/',
    'http://172.16.0.1/',
    'http://192.168.1.2/',
    'http://169.254.169.254/latest/meta-data/',
    'http://[::1]/',
    'http://[fd00::1]/',
    'http://user:secret@example.com/',
  ])('blocks unsafe URL %s', (url) => {
    expect(() => assertPublicHttpUrl(url)).toThrow(TypeError);
  });

  it('allows public WebSocket URLs and blocks private WebSocket targets', () => {
    expect(assertPublicWebSocketUrl('wss://example.com/socket').protocol).toBe('wss:');
    expect(() => assertPublicWebSocketUrl('ws://127.0.0.1/socket')).toThrow(TypeError);
    expect(() => assertPublicWebSocketUrl('https://example.com/socket')).toThrow(TypeError);
  });
});
