import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [react(), tailwindcss()],
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
