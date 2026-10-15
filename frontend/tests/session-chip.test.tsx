// The header chip for a live session this page did not start says what it
// knows: a different board, or the same file started by another tab or an
// earlier page load. Hover shows the one-line meaning and the two actions.

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { ShellChips } from '../src/components/ShellChips'
import type { WebReport } from '../src/types/report'

const noop = () => {}

const chip = (sessionBoard: string, currentBoard: string) =>
  renderToStaticMarkup(
    <ShellChips
      view="board"
      report={{ ok: true, serious: 0, total: 0 } as unknown as WebReport}
      checks={null}
      sim={{ running: false, faults: 0, connected: false, sessionBoard: null } as never}
      simMounted={false}
      simOffline={false}
      sessionBoard={sessionBoard}
      sessionMatchesCurrent={false}
      currentBoard={currentBoard}
      serverLiveActive
      goTo={noop}
      onOpenLiveSession={noop}
      onDriveLive={noop}
    />,
  )

describe('another-session chip', () => {
  test('names a different board as another session', () => {
    expect(chip('other.board', 'mine.board')).toContain('another session is live: other.board')
  })

  test('does not call a same-named session "another": it may be this board from another tab', () => {
    const html = chip('mine.board', 'mine.board')
    expect(html).toContain('live session not from this tab: mine.board')
    expect(html).not.toContain('another session is live')
  })
})
