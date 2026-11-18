import { defineConfig } from 'vitest/config';
import { assetShimPlugin } from './test/helpers/vite-assets.ts';

export default defineConfig({
  plugins: [assetShimPlugin()],
  test: {
    include: ['test/**/*.test.ts'],
    environment: 'node',
    setupFiles: ['test/setup.ts'],
  },
});
