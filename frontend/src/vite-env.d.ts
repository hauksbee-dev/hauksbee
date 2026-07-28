/// <reference types="vite/client" />

/** Build-time literal from vite.config.ts `define`: true only in the
 *  VITE_DEMO=1 demo build. Tests and other non-Vite entry points never see
 *  it, so nothing outside main.tsx may reference it. */
declare const __DEMO__: boolean
