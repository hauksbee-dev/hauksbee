// The one place the app says which version of hauksbee it is, and therefore
// which published artifacts belong to it.
//
// `__APP_VERSION__` is a build-time literal injected by vite.config.ts from
// frontend/package.json, whose version tracks the workspace Cargo.toml. That
// indirection is the point: the GitHub action reference below used to carry a
// hand-typed tag, which is exactly the kind of string that survives three
// releases after it stopped being true. A generated workflow that pins a tag
// nobody published fails on the user's first push, and the failure blames
// their spec rather than our copy.

export const APP_VERSION: string = __APP_VERSION__

/** The release tag for this build, in the form the git tags use. */
export const RELEASE_TAG = `v${APP_VERSION}`

/**
 * The published GitHub action, pinned to this build's release tag rather than
 * a moving branch: a workflow generated today must not change behavior when
 * main moves.
 *
 * The tag itself is created by the release workflow
 * (.github/workflows/release.yml) at launch, from the same version this
 * constant reads. Until that first release runs, the tag named here does not
 * resolve yet, which is why the generated YAML carries a line telling the
 * reader to match the hauksbee they actually have installed.
 */
export const ACTION_REF = `hauksbee-dev/hauksbee/integrations/github-action@${RELEASE_TAG}`
