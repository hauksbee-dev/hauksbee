import { Component, type ErrorInfo, type ReactNode } from 'react'

interface Props {
  children: ReactNode
}

interface State {
  error: Error | null
}

/** Keep an unexpected report-rendering exception from turning the whole app
 * into an unexplained blank page. This boundary deliberately does not clear
 * sessions or uploads: reload is safe, and the diagnostic remains visible. */
export class AppErrorBoundary extends Component<Props, State> {
  state: State = { error: null }

  static getDerivedStateFromError(error: Error): State {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('Hauksbee display error', error, info.componentStack)
  }

  render() {
    if (!this.state.error) return this.props.children
    return (
      <main className="min-h-screen flex items-center justify-center p-6" style={{ background: 'var(--canvas)', color: 'var(--silk)' }}>
        <section
          role="alert"
          className="w-full max-w-xl rounded-xl border p-6"
          style={{ borderColor: 'var(--bad)', background: 'var(--panel)' }}
        >
          <p className="text-xs uppercase tracking-[0.18em]" style={{ color: 'var(--bad)' }}>Display error</p>
          <h1 className="mt-2 text-xl font-semibold">Hauksbee could not draw this view</h1>
          <p className="mt-3 text-sm" style={{ color: 'var(--silk-dim)' }}>
            Your board and saved sessions have not been deleted. Reload the local app and try the upload again.
          </p>
          <pre className="mt-4 overflow-auto rounded-lg border p-3 text-xs" style={{ borderColor: 'var(--line)', background: 'var(--well)' }}>
            {this.state.error.message || this.state.error.name}
          </pre>
          <button
            type="button"
            className="mt-5 rounded-lg px-4 py-2 text-sm font-medium"
            style={{ background: 'var(--accent)', color: 'var(--accent-ink)' }}
            onClick={() => window.location.reload()}
          >
            Reload Hauksbee
          </button>
        </section>
      </main>
    )
  }
}
