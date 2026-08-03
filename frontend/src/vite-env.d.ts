/// <reference types="vite/client" />

/** Build-time literal from vite.config.ts `define`: true only in the
 *  VITE_DEMO=1 demo build. Tests and other non-Vite entry points never see
 *  it, so nothing outside main.tsx may reference it. */
declare const __DEMO__: boolean

/** Build-time literal from vite.config.ts `define`: this frontend's version,
 *  read out of package.json at build time so the app cannot claim a version
 *  the package does not have. See src/lib/version.ts for what reads it. */
declare const __APP_VERSION__: string
