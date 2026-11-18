/**
 * Vitest global setup: polyfills the handful of Worker globals that the app
 * source relies on (`btoa`/`atob` are available on Node 20 already, but we
 * make sure a consistent implementation exists) and keeps console noise down.
 */

if (typeof globalThis.btoa !== 'function') {
  globalThis.btoa = (binary: string) => Buffer.from(binary, 'binary').toString('base64');
}
if (typeof globalThis.atob !== 'function') {
  globalThis.atob = (value: string) => Buffer.from(value, 'base64').toString('binary');
}

export {};
