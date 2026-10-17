// Builds the SDK as one ES module (dist/cortex-sdk.js). React stays external: the workbench
// serves one React to everything through its import map.
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  define: { 'process.env.NODE_ENV': JSON.stringify('production') },
  build: {
    outDir: 'dist',
    emptyOutDir: false,
    target: 'es2023',
    minify: true,
    sourcemap: true,
    lib: { entry: 'src/index.ts', formats: ['es'], fileName: () => 'cortex-sdk.js' },
    rolldownOptions: { external: ['react', 'react/jsx-runtime', 'react-dom', 'react-dom/client'] },
  },
  test: { include: ['test/**/*.test.{ts,tsx}'], environment: 'node' },
});
