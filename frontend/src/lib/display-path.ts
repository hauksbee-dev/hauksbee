// Home directories on screen. The server reports real absolute paths; shown
// as-is they put the user's account name in every screenshot and bug report.
// This is display only: callers keep the real path in `title` and for copying.

const HOME_PREFIX = /(?<![\w.~-])(?:\/Users\/[^/\s]+|\/home\/[^/\s]+|[A-Za-z]:\\Users\\[^\\\s]+)(?=[/\\]|$|\s)/g

/** `text` with any macOS, Linux or Windows home directory written as `~`. */
export function tildeHome(text: string): string {
  return text.replace(HOME_PREFIX, '~')
}
