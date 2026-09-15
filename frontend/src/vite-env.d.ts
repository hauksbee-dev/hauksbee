/// <reference types="vite/client" />

/** Build-time literal from vite.config.ts `define`: this frontend's version,
 *  read out of package.json at build time so the app cannot claim a version
 *  the package does not have. See src/lib/version.ts for what reads it. */
declare const __APP_VERSION__: string

/** Exact release commit injected by release/CI builds. Empty dev builds refuse
 * to generate a credential-bearing workflow until a commit is supplied. */
declare const __RELEASE_COMMIT__: string
