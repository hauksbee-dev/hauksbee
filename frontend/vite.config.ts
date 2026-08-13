import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import pkg from './package.json' with { type: 'json' }

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // Build-time constant, not an import.meta.env lookup: only a literal is
  // constant-folded, which is what removes the demo shell from the live
  // bundle and the upload/live machinery from the demo bundle.
  define: {
    __DEMO__: JSON.stringify(process.env.VITE_DEMO === '1'),
    // The version the app reports (and pins the published GitHub action to),
    // read from package.json here so no source file carries a copy of it.
    __APP_VERSION__: JSON.stringify(pkg.version),
    // A released web bundle is built from a clean tagged commit. Emitting that
    // immutable object ID lets generated consumer workflows fetch the exact
    // private Action code instead of trusting a movable Git tag with a token.
    __RELEASE_COMMIT__: JSON.stringify(process.env.HAUKSBEE_RELEASE_COMMIT || ''),
  },
  server: {
    proxy: {
      '/ws': {
        target: 'ws://localhost:3001',
        ws: true,
      },
      // Without these, `vite dev` answers /api/startup itself with index.html
      // (a 200 whose body is not JSON) and swallows /api/analyze POSTs, so the
      // dev app silently diverges from the served app. Proxy everything the
      // backend owns to it, same target as /ws.
      '/api': { target: 'http://localhost:3001' },
      '/boards': { target: 'http://localhost:3001' },
    },
  },
})
