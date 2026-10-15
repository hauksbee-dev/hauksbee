import { useEffect, useState } from 'react'
import type { BoardSession } from '../hooks/useBoardSession'
import { acceptedFormatsSentence, withoutEngineFormatList } from '../lib/board-formats'
import { parseLocation, snippetAt } from '../lib/parse-snippet'
import type { Snippet } from '../lib/parse-snippet'
import { useDropTarget } from '../motion'
import type { WebReport } from '../types/report'
import { Callout } from './ui'

// A board the engine could not read, shown where the file was dropped: the
// same card shape as the landing drop zone, with what went wrong, the text
// around the place it stopped, and ONE way to try again (click, or drop another
// file on it). The header's "Analyze another board" is absent for this state so
// there is a single retry.

/** The "choose a file" button: a label for the app's one hidden board input. */
export function DropBoardAgain({ testId, label, height }: { testId: string; label: string; height: number }) {
  return (
    <label
      htmlFor="board-file"
      data-testid={testId}
      role="button"
      tabIndex={0}
      onKeyDown={e => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          document.getElementById('board-file')?.click()
        }
      }}
      className="hb-btn-primary hb-press inline-flex items-center px-3.5 text-[13px] cursor-pointer"
      style={{ height }}
    >
      {label}
    </label>
  )
}

export function UploadFailure({ report, session }: { report: WebReport; session: BoardSession }) {
  const drop = useDropTarget(files => { if (files[0]) session.handleBoard(files[0]) })
  const message = report.error ? withoutEngineFormatList(report.error) : 'Could not read the file.'
  const failure = report.import_failure
  const [snippet, setSnippet] = useState<Snippet | null>(null)
  const file = session.boardFile

  // The engine's excerpt wins when it sent one; otherwise read the file around
  // the offset the message names.
  useEffect(() => {
    setSnippet(null)
    const loc = report.error ? parseLocation(report.error) : null
    if (failure?.excerpt || !loc || !file) return
    let live = true
    void snippetAt(file, loc).then(found => { if (live) setSnippet(found) }).catch(() => undefined)
    return () => { live = false }
  }, [report.error, failure?.excerpt, file])

  return (
    <div className="h-full overflow-y-auto view-enter">
      <div className="max-w-xl mx-auto px-6 pt-10 pb-16">
        <div
          data-testid="upload-failure"
          {...drop.bind}
          className="drop-card px-6 py-8"
          data-active={drop.state === 'over' ? 'true' : 'false'}
        >
          <div className="text-center text-[17px] font-semibold" style={{ color: 'var(--silk)' }}>
            That file did not load
          </div>
          <Callout tone="err" testId="report-verdict" className="mt-4 px-4 py-3.5">
            {message}
          </Callout>
          {snippet && (
            <pre
              data-testid="parse-snippet"
              className="mt-3 overflow-x-auto rounded px-3 py-2 text-[11px] leading-snug"
              style={{ background: 'var(--canvas)', border: '1px solid var(--hairline)', color: 'var(--silk)' }}
            >
              {`${snippet.lineNo} | ${snippet.text}`}
              {snippet.caret !== null && `\n${' '.repeat(`${snippet.lineNo} | `.length + snippet.caret)}^`}
            </pre>
          )}
          {failure && (
            <Callout tone="warn" testId="import-failure" className="mt-3" title={`Import stopped at ${failure.stage}`}>
              {failure.excerpt && (
                <pre className="mt-2 overflow-x-auto rounded px-3 py-2 text-[11px]" style={{ background: 'var(--canvas)', border: '1px solid var(--hairline)' }}>
                  {failure.excerpt}
                </pre>
              )}
              <div className="mt-2"><b>Suggested fix:</b> {failure.suggested_fix}</div>
            </Callout>
          )}
          <div className="mt-5 flex flex-col items-center gap-2 text-center">
            <DropBoardAgain testId="try-another-file" label="Choose another file" height={34} />
            <span className="text-[12px] leading-relaxed" style={{ color: 'var(--silk-faint)' }}>
              or drop one here. Accepted: {acceptedFormatsSentence()}
            </span>
          </div>
        </div>
      </div>
    </div>
  )
}
