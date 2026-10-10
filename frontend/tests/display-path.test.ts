import { expect, test } from 'bun:test'
import { tildeHome } from '../src/lib/display-path'

test('a home directory is shown as ~ and the rest of the path is kept', () => {
  expect(tildeHome('/Users/jane/.config/hauksbee/extract.toml')).toBe('~/.config/hauksbee/extract.toml')
  expect(tildeHome('/home/jane/renode-portable/renode')).toBe('~/renode-portable/renode')
  expect(tildeHome('C:\\Users\\jane\\renode\\renode.exe')).toBe('~\\renode\\renode.exe')
  expect(tildeHome('/Users/jane')).toBe('~')
})

test('home paths inside a sentence are abbreviated, other paths are untouched', () => {
  expect(tildeHome('from /Users/jane/x.toml (nothing saved yet)')).toBe('from ~/x.toml (nothing saved yet)')
  expect(tildeHome('/opt/homebrew/bin/renode')).toBe('/opt/homebrew/bin/renode')
  expect(tildeHome('/tmp/Users/jane/x')).toBe('/tmp/Users/jane/x')
  expect(tildeHome('~/already/short')).toBe('~/already/short')
})
