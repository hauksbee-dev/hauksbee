// The build-time literals vite injects, installed once for the whole suite.
//
// src/lib/version.ts reads them at module evaluation, which happens on the
// first import anywhere in the run. Declaring them per test file makes the
// values a test asserts on depend on the order bun loaded the files in, so
// they are declared here and every file that needs them imports this.

export const APP_VERSION = '0.1.0'
export const RELEASE_TAG = `v${APP_VERSION}`
export const RELEASE_COMMIT = '0123456789abcdef0123456789abcdef01234567'

Object.assign(globalThis, {
  __APP_VERSION__: APP_VERSION,
  __RELEASE_COMMIT__: RELEASE_COMMIT,
  __RELEASE_TAG__: RELEASE_TAG,
})
