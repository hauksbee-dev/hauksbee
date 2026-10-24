import pkg from '../package.json' with { type: 'json' }

Object.assign(globalThis, {
  __APP_VERSION__: pkg.version,
  __RELEASE_COMMIT__: '0123456789abcdef0123456789abcdef01234567',
})
