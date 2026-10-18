import { useEffect, useState } from 'react'
import type { BoardSession } from '../hooks/useBoardSession'
import { acceptedFormatsSentence, withoutEngineFormatList } from '../lib/board-formats'
import { parseLocation, snippetAt } from '../lib/parse-snippet'
import type { Snippet } from '../lib/parse-snippet'
import { useDropTarget } from '../motion'
import type { WebReport } from '../types/report'
import { Callout } from './ui'
import { FindFilesHelp, SampleRow } from './UploadView'

// A board the engine could not read, shown where the file was dropped: the
// same card shape as the landing drop zone, with what went wrong in plain
// words, the exact thing to do about it, and ONE way to try again (click, or
// drop another file on it). The engine's own message and the text around the
// place it stopped are kept, behind "Technical detail", because they are what a
// bug report needs and what a person does not. The header's "Analyze another
// board" is absent for this state so there is a single retry.

/** The "choose a file" button: a label for the app's one hidden board input. */
export function DropBoardAgain({ testId, label, height, primary = true }: { testId: string; label: string; height: number; primary?: boolean }) {
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
      className={`${primary ? 'hb-btn-primary' : 'hb-btn'} hb-press inline-flex items-center px-3.5 text-[13px] cursor-pointer`}
      style={{ height }}
    >
      {label}
    </label>
  )
}

/** A refusal in a person's terms: what is wrong with the file, and what to do. */
export interface FailureExplanation {
  title: string
  /** One sentence naming the file and what is wrong with it. */
  cause: string
  /** Concrete steps, in order. Never empty. */
  steps: string[]
}

const extensionOf = (name: string): string => {
  const i = name.lastIndexOf('.')
  return i < 0 ? '' : name.slice(i).toLowerCase()
}

const RESAVE = 'Open the project in the tool that made it, save it again (KiCad: File > Save), and drop the new file here.'

/**
 * Explain a refused board. The cause is read from the engine's message (the
 * engine sends prose, not a code, so this matches on the phrases it uses for
 * the two failures people actually hit) and from the file's own name; any other
 * refusal falls through to the engine's sentence with generic next steps, never
 * to nothing. Exported so the wording is testable without rendering.
 */
export function explainBoardFailure(message: string, fileName: string): FailureExplanation {
  const name = `“${fileName}”`
  const ext = extensionOf(fileName)
  const isKicadBoard = ext === '.kicad_pcb'

  // The file stops inside a bracket or a string: cut off, not malformed.
  if (/unclosed (list|string)|unexpected end|premature end|ends? early|truncated/i.test(message)) {
    return {
      title: 'This file ends early',
      cause: `${name} stops in the middle of its contents, so it looks cut off. That usually means a save, copy or download was interrupted.`,
      steps: [
        RESAVE,
        'If you copied or downloaded it, copy it again and check the size matches the original.',
      ],
    }
  }

  // The contents are not this format at all.
  if (/unrecognized board format/i.test(message)) {
    if (isKicadBoard) {
      return {
        title: 'This is not a KiCad board',
        cause: `${name} ends in .kicad_pcb, but a KiCad board starts with “(kicad_pcb”, and this file does not. It may be a different file that was renamed, or an export that came out empty.`,
        steps: [
          'Open the project in KiCad, open the PCB editor, and save the board (File > Save).',
          'Drop that .kicad_pcb from your project folder, not the .kicad_pro project file or a PDF.',
        ],
      }
    }
    return {
      title: ext ? `Hauksbee cannot read a ${ext} file` : 'Hauksbee cannot read this file',
      cause: ext
        ? `${name} is not one of the board formats hauksbee reads, or its contents do not match what a ${ext} file should hold.`
        : `${name} has no file extension, and its contents are not any board format hauksbee reads.`,
      steps: [
        'Drop the board design itself: for KiCad the .kicad_pcb in your project folder, for Eagle the .brd, for Altium the .PcbDoc.',
        'Only have fab files? Zip the folder holding the gerbers and the drill file and drop the zip.',
      ],
    }
  }

  // A syntax error at a place the engine names.
  if (/parse error at/i.test(message)) {
    const loc = parseLocation(message)
    const where = loc?.line != null ? `line ${loc.line}` : 'the place shown under Technical detail'
    return {
      title: 'This file has a syntax error',
      cause: `${name} could not be read past ${where}. A hand edit or a bad merge can break the file there.`,
      steps: [
        `Open the file at ${where} and fix or undo the last edit made there.`,
        `Or ${RESAVE.charAt(0).toLowerCase()}${RESAVE.slice(1)}`,
      ],
    }
  }

  // Anything else the engine refused: say it, and still say what to try.
  return {
    title: 'That file did not load',
    cause: `${withoutEngineFormatList(message).replace(/^Could not read this board file:\s*/i, '')}`,
    steps: [
      RESAVE,
      'Still stuck? Open Technical detail and include it when you report the file.',
    ],
  }
}

export function UploadFailure({ report, session }: { report: WebReport; session: BoardSession }) {
  const drop = useDropTarget(files => { if (files[0]) session.handleBoard(files[0]) })
  const message = report.error ? withoutEngineFormatList(report.error) : 'Could not read the file.'
  const failure = report.import_failure
  const [snippet, setSnippet] = useState<Snippet | null>(null)
  const file = session.boardFile
  const explained = explainBoardFailure(message, file?.name ?? report.file_name ?? 'this file')

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
            {explained.title}
          </div>
          <Callout tone="err" testId="report-verdict" className="mt-4 px-4 py-3.5">
            <p style={{ margin: 0 }}>{explained.cause}</p>
            <div className="mt-3 text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--err-strong)' }}>
              What to do
            </div>
            <ol className="mt-1 space-y-1 pl-5" style={{ listStyle: 'decimal' }}>
              {explained.steps.map(step => <li key={step}>{step}</li>)}
            </ol>
            <details data-testid="failure-detail" className="mt-3 text-[12px]">
              <summary className="cursor-pointer">Technical detail</summary>
              <div className="mt-2" style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-word' }}>{message}</div>
              {snippet && (
                <pre
                  data-testid="parse-snippet"
                  className="mt-2 overflow-x-auto rounded px-3 py-2 text-[11px] leading-snug"
                  style={{ background: 'var(--canvas)', border: '1px solid var(--hairline)', color: 'var(--silk)' }}
                >
                  {`${snippet.lineNo} | ${snippet.text}`}
                  {snippet.caret !== null && `\n${' '.repeat(`${snippet.lineNo} | `.length + snippet.caret)}^`}
                </pre>
              )}
            </details>
          </Callout>
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
        {/* The way out stays whole: a sample gives a report with no file at
            all, and the file-finding help is the same as on the first screen. */}
        <SampleRow onRun={session.runSample} avrAvailable={false} />
        <FindFilesHelp />
      </div>
    </div>
  )
}
