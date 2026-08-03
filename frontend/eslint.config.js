// eslint 9 flat config for the frontend.
//
// What this is for: the mistakes `tsc` cannot see. A hook called after an early
// return, a promise nobody waits for, an async function handed to an attribute
// that discards its result. It is deliberately NOT a style tool: this codebase's
// formatting is consistent by hand, and a formatter's opinions here would be
// noise piled on top of real findings.
//
//   bun run lint
//
// Two rulesets are deliberately left out.
//
// typescript-eslint's `recommendedTypeChecked` in full: most of what it adds
// here is `no-unsafe-*` and `no-base-to-string` firing where our types meet
// something genuinely untyped (a parsed TOML document, a JSON response, three.js
// internals). That is where the casts live on purpose, so the rule would be
// permanently red for structural reasons rather than finding anything. The
// type-aware rules that DO find bugs are switched on individually below.
//
// eslint-plugin-react-hooks 7's `recommended-latest`: that is the React Compiler
// ruleset (purity, immutability, refs, set-state-in-effect,
// preserve-manual-memoization). This app does not run the compiler, and those
// rules judge code against a contract it never agreed to: `Date.now()` in a state
// initializer and a `useRef` written from an event handler are correct
// non-compiled React and there are dozens of them. If the compiler is ever
// adopted, turning that config on is the first step of the migration, not a lint
// tidy-up. The two rules that apply to any React are on.
//
// Not wired into CI yet: green here, but a warning should not silently pass a
// gate, and `exhaustive-deps` is deliberately a warning. Decide that first.

import js from '@eslint/js'
import globals from 'globals'
import tseslint from 'typescript-eslint'
import reactHooks from 'eslint-plugin-react-hooks'

/** Type-aware rules worth the type information, and reachable at zero. */
const typeAware = {
  // A promise nobody waits for swallows its own failure.
  '@typescript-eslint/no-floating-promises': 'error',
  // An async function in an `onClick` returns a promise the DOM throws away, so
  // a rejection inside it is unhandled. `onClick={() => void save()}` is how the
  // codebase says "fire and forget, on purpose".
  '@typescript-eslint/no-misused-promises': 'error',
  '@typescript-eslint/await-thenable': 'error',
  '@typescript-eslint/no-for-in-array': 'error',
  '@typescript-eslint/no-array-delete': 'error',
  '@typescript-eslint/no-duplicate-type-constituents': 'error',
}

/** `_`-prefixed is the codebase's existing way of saying "required by the
 *  signature, deliberately unused"; tsconfig's noUnusedLocals agrees. */
const unusedVars = {
  '@typescript-eslint/no-unused-vars': [
    'error',
    { argsIgnorePattern: '^_', varsIgnorePattern: '^_', caughtErrors: 'none' },
  ],
}

export default tseslint.config(
  // Build output and dependencies. Also `public/`: it holds sample boards and
  // multi-MB GLB models, and pointing a parser at those is how you hang it.
  {
    ignores: ['dist/**', 'node_modules/**', 'public/**', 'test-results/**'],
  },

  // ── The app: browser globals, React's hook rules ──────────────────────────
  {
    files: ['src/**/*.{ts,tsx}'],
    extends: [js.configs.recommended, ...tseslint.configs.recommended],
    plugins: { 'react-hooks': reactHooks },
    languageOptions: {
      ecmaVersion: 2023,
      globals: globals.browser,
      parserOptions: {
        project: ['./tsconfig.app.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      ...unusedVars,
      ...typeAware,
      // Hooks after a conditional return change the hook count between renders,
      // which is a crash, not a smell.
      'react-hooks/rules-of-hooks': 'error',
      // A warning on purpose, and read every time. Four remain, all of them a
      // judgement about a callback prop or a memo that is deliberately not keyed
      // on everything it closes over; each has the reason written above it, and
      // a per-site disable comment would bury that reason under boilerplate.
      'react-hooks/exhaustive-deps': 'warn',
      // Bare `any` has one narrow home here: the WebGL / three.js and canvas
      // seams where vendor types meet ours. Worth seeing, not worth failing.
      '@typescript-eslint/no-explicit-any': 'warn',
    },
  },

  // ── Node/bun scripts: the mock server, the visual-lint harness, the tests ──
  {
    files: ['tests/**/*.ts', 'mock-server.ts', 'vite.config.ts'],
    extends: [js.configs.recommended, ...tseslint.configs.recommended],
    languageOptions: {
      ecmaVersion: 2023,
      // The harness serializes `auditPage` into a browser, so its files
      // legitimately reference `window` and `document` beside bun's own globals.
      globals: { ...globals.node, ...globals.browser },
      parserOptions: {
        project: ['./tsconfig.tests.json', './tsconfig.node.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: { ...unusedVars, ...typeAware },
  },

  // This file itself: plain ESM config with no type information to attach.
  {
    files: ['eslint.config.js'],
    extends: [js.configs.recommended],
    languageOptions: { ecmaVersion: 2023, globals: globals.node },
  },
)
