import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { resolve } from 'node:path'
import { readFileSync } from 'node:fs'

// The embeddable widget's own build. Separate from the frontend's config on
// purpose: the live app's bundle is frontend/dist (served by the engine binary)
// and the site demo's is demo/dist. This one writes demo/embed-dist and must
// never touch either.
//
// Two entries:
//   hauksbee-embed.js  the host-page API (tiny; no React, no app). It loads the
//                      widget lazily, so a landing page that never scrolls to
//                      the demo pays for nothing but this file.
//   iframe.html        the isolated shape: a document that mounts the widget and
//                      bridges the contract over postMessage.
//
// Node resolution: demo/ has no node_modules of its own. build.sh links
// frontend's in, which is also where the app's own dependencies (react, motion,
// three, smol-toml) have to come from so there is exactly one copy of React.

const here = import.meta.dirname
const FRONTEND = resolve(here, '../../frontend')
const pkg = JSON.parse(readFileSync(resolve(FRONTEND, 'package.json'), 'utf8')) as { version: string }

export default defineConfig({
  root: here,
  plugins: [react(), tailwindcss()],
  // Relative asset URLs: the widget is copied under whatever path the host site
  // gives it, and an absolute /assets/... would only work at a site root.
  base: './',
  define: {
    // The app's build-time literals. __DEMO__ false: this is not the demo
    // shell's build, it is the real app with a cached transport behind it.
    __DEMO__: 'false',
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  resolve: {
    alias: {
      // One React, whatever the import path walked up to.
      react: resolve(FRONTEND, 'node_modules/react'),
      'react-dom': resolve(FRONTEND, 'node_modules/react-dom'),
    },
    dedupe: ['react', 'react-dom'],
  },
  build: {
    outDir: resolve(here, '../embed-dist'),
    emptyOutDir: true,
    // The app's own chunking already splits the 3D viewer (three.js) out behind
    // a dynamic import; keep that, and keep the widget out of the host module.
    rollupOptions: {
      // hauksbee-embed.js is a MODULE a host imports, not a page bundle: without
      // this the bundler drops its exports (an app entry has none) and the host
      // gets "does not provide an export named createHauksbeeDemo".
      preserveEntrySignatures: 'strict',
      input: {
        'hauksbee-embed': resolve(here, 'embed.ts'),
        iframe: resolve(here, 'iframe.html'),
      },
      output: {
        entryFileNames: '[name].js',
        chunkFileNames: 'chunks/[name]-[hash].js',
        assetFileNames: 'assets/[name]-[hash][extname]',
      },
    },
    // A widget on someone else's page: keep the sourcemap so a console error in
    // it is debuggable, but do not ship it as a comment the host has to serve.
    sourcemap: false,
    target: 'es2022',
    cssCodeSplit: false,
    reportCompressedSize: true,
  },
})
