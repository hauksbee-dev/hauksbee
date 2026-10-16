// Engine prose that points at documentation carries the full docs URL (the
// engine renders every doc pointer through its one docs_url transform). This
// turns those URLs into links, so a "see ..." in a detail line is clickable and
// the page never builds or guesses a docs address itself.

const URL_RE = /(https?:\/\/[^\s)]+)/g

export function LinkedText({ text }: { text: string }) {
  return (
    <>
      {text.split(URL_RE).map((part, i) =>
        i % 2 === 1
          ? <a key={i} href={part} target="_blank" rel="noreferrer" style={{ color: 'var(--copper-hi)', textDecoration: 'underline' }}>{part.replace(/^https?:\/\//, '')}</a>
          : part,
      )}
    </>
  )
}
