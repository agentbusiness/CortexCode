// The renderer core's own tests (plugins and the SDK test themselves in their packages).
import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  test: { include: ['test/**/*.test.{ts,tsx}'], environment: 'node' },
});
