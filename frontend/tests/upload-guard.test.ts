import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

const {
  MAX_UPLOAD_BYTES,
  analysisFailureMessage,
  precheckBoardFile,
} = await import('../src/lib/upload-guard')

test('an oversized board is refused from metadata before any byte can be read', () => {
  const touched: PropertyKey[] = []
  const file = new Proxy({
    name: 'huge-layout.kicad_pcb',
    size: MAX_UPLOAD_BYTES + 1,
  }, {
    get(target, key, receiver) {
      if (key !== 'name' && key !== 'size') touched.push(key)
      return Reflect.get(target, key, receiver)
    },
  }) as File

  const refusal = precheckBoardFile(file)
  expect(refusal).toContain('256.0 MB')
  expect(refusal).toContain('accepts up to 256 MB')
  expect(touched).toEqual([])
})

test('the exact server limit remains admissible and the first byte beyond it refuses', () => {
  const atLimit = { name: 'large.kicad_pcb', size: MAX_UPLOAD_BYTES } as File
  const overLimit = { name: 'large.kicad_pcb', size: MAX_UPLOAD_BYTES + 1 } as File
  expect(precheckBoardFile(atLimit)).toBeNull()
  expect(precheckBoardFile(overLimit)).not.toBeNull()
})

test('a large unknown format refuses while a small mystery file reaches the extractor', () => {
  expect(precheckBoardFile({ name: 'mystery.bin', size: 21 * 1024 * 1024 } as File))
    .toContain('does not look like a board file')
  expect(precheckBoardFile({ name: 'mystery.bin', size: 1024 } as File)).toBeNull()
})

test('body-limit and transport failures remain distinct actionable messages', () => {
  const tooLarge = analysisFailureMessage(new Error('length limit exceeded'), {
    status: 413,
    size: MAX_UPLOAD_BYTES + 1,
  })
  expect(tooLarge).toContain('server refused this board as too large')
  expect(tooLarge).toContain('256 MB per upload')

  const disconnected = analysisFailureMessage(new TypeError('Failed to fetch'), {
    size: 40 * 1024 * 1024,
  })
  expect(disconnected).toContain('connection to the server dropped')
  expect(disconnected).toContain('near the 256 MB upload limit')
})

test('the board hook checks metadata before routing and streams the File body', () => {
  const source = readFileSync(
    new URL('../src/hooks/useBoardSession.ts', import.meta.url),
    'utf8',
  )
  const handleStart = source.indexOf('const handleBoard = useCallback((f: File) => {')
  const handleEnd = source.indexOf('\n  }, [acceptBoard, busy, clearRunState, handleFirmware])', handleStart)
  const handle = source.slice(handleStart, handleEnd)
  expect(handle.indexOf('const refusal = precheckBoardFile(f)')).toBeGreaterThanOrEqual(0)
  expect(handle.indexOf('const refusal = precheckBoardFile(f)'))
    .toBeLessThan(handle.indexOf('acceptBoard(f)'))

  const analyzeStart = source.indexOf('const analyze = useCallback(async (')
  const analyzeEnd = source.indexOf('\n  }, [beginRun, clearRunState])', analyzeStart)
  const analyze = source.slice(analyzeStart, analyzeEnd)
  expect(analyze).toContain('body: board,')
  expect(analyze).not.toContain('body: await board.arrayBuffer()')
})
