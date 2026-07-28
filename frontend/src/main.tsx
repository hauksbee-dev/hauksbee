import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App'
import DemoApp from './demo/DemoApp'

// VITE_DEMO=1 builds the hauksbee.dev demo shell (recorded-session replays);
// everything else is the live app. __DEMO__ is a build-time literal (see
// vite.config.ts), so the dead branch and its module graph are eliminated
// from the other build.
const DEMO = __DEMO__

const root = document.getElementById('root')!
createRoot(root).render(
  <StrictMode>
    {DEMO ? <DemoApp /> : <App />}
  </StrictMode>
)
