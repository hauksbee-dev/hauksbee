// Reading a Server-Sent Events body from `fetch`.
//
// Two endpoints stream progress this way (dependency installs and datasheet
// extraction) because both run for minutes and a request that blocks silently
// until the end is indistinguishable from a hang. `EventSource` is not an option
// for either: both are POSTs. So the body is read by hand, and this is the one
// copy of that code, because the frame splitting is the part that goes subtly
// wrong (a frame split across two chunks, a `data:` line with no space, a
// multi-line payload) and two copies drift in different ways.

/** One parsed SSE frame: the text between blank lines. */
export interface SseFrame {
  event: string
  data: string
}

/** Parse one frame's raw text. Defaults to the `log` event, which is what the
 *  servers emit for an unlabelled progress line. */
export function parseSseFrame(raw: string): SseFrame {
  let event = 'log'
  const data: string[] = []
  for (const line of raw.split('\n')) {
    if (line.startsWith('event: ')) event = line.slice(7).trim()
    else if (line.startsWith('data: ')) data.push(line.slice(6))
    else if (line === 'data:') data.push('')
  }
  return { event, data: data.join('\n') }
}

/** Read an SSE response body to the end, calling `onFrame` for each frame.
 *
 *  Returns when the stream closes. It does NOT decide what a missing terminal
 *  event means: a caller that expects a final `done`/`error` frame has to notice
 *  it never arrived, because "the connection dropped mid-install" and "the
 *  install failed" need different words. */
export async function readSseStream(
  body: ReadableStream<Uint8Array>,
  onFrame: (frame: SseFrame) => void,
): Promise<void> {
  const reader = body.getReader()
  const dec = new TextDecoder()
  let buf = ''
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    buf += dec.decode(value, { stream: true })
    let idx: number
    while ((idx = buf.indexOf('\n\n')) >= 0) {
      const frame = parseSseFrame(buf.slice(0, idx))
      buf = buf.slice(idx + 2)
      onFrame(frame)
    }
  }
}
