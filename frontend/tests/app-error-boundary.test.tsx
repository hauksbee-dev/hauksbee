import { expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { AppErrorBoundary } from '../src/components/AppErrorBoundary'

test('an unexpected render error becomes a recoverable visible diagnostic', () => {
  const boundary = new AppErrorBoundary({ children: null })
  boundary.state = AppErrorBoundary.getDerivedStateFromError(new Error('board renderer exploded'))
  const html = renderToStaticMarkup(boundary.render())
  expect(html).toContain('role="alert"')
  expect(html).toContain('Hauksbee could not draw this view')
  expect(html).toContain('board renderer exploded')
  expect(html).toContain('Reload Hauksbee')
})
