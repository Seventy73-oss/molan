import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

// 开发期代理到本地 Rust 服务（单端口托管时不需要）；最终交付是 dist/ 静态树，由 molan-server 托管。
const backend = process.env.MOLAN_BACKEND ?? 'http://127.0.0.1:17381';

export default defineConfig({
  plugins: [react()],
  base: '/',
  build: {
    outDir: 'dist',
    assetsDir: 'assets',
    sourcemap: false,
    target: 'es2022',
    chunkSizeWarningLimit: 900,
  },
  server: {
    port: 5173,
    proxy: {
      '/ipc': { target: backend, changeOrigin: false },
      '/auth': { target: backend, changeOrigin: false },
      '/login': { target: backend, changeOrigin: false },
      '/health': { target: backend, changeOrigin: false },
    },
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
