import type { Plugin } from 'vite';
import path from 'node:path';

export function assetShimPlugin(): Plugin {
  return {
    name: 'servo-test-asset-shim',
    enforce: 'pre',
    resolveId(source) {
      if (source.endsWith('/vendor/servo-worker/worker-adapter.mjs')) {
        return path.resolve(process.cwd(), 'test/helpers/servo-worker-adapter.ts');
      }
      return source.endsWith('.wasm') ? '\0servo-test-wasm:' + source : null;
    },
    load(id) {
      return id.startsWith('\0servo-test-wasm:') ? 'export default {};' : null;
    },
  };
}
