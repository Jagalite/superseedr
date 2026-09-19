import {defineConfig} from 'vite';
export default defineConfig({
  build: {outDir: 'client-dist', assetsDir: 'client-assets', rollupOptions: {input: 'webtorrent.html'}},
  worker: {format: 'es'},
});
