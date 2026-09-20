import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { AdditionalEvidencePanel } from '../src/components/AdditionalEvidencePanel'

/** Every slot empty and every handler a no-op: these tests assert what the
 *  panel RENDERS, not the session wiring behind it. The two clear handlers are
 *  optional and are passed only by the test that needs them present. */
const EMPTY = {
  firmware: null,
  schematic: null,
  bom: null,
  placementFile: null,
  variant: null,
  asbuilt: null,
  models: [],
  onFirmware: () => {},
  onSchematic: () => {},
  onBom: () => {},
  onPlacement: () => {},
  onVariant: () => {},
  onAsbuilt: () => {},
  onModels: () => {},
}

describe('additional evidence surface', () => {
  test('offers the complete supplemental-input contract in the browser', () => {
    const html = renderToStaticMarkup(
      <AdditionalEvidencePanel
        placement="intake"
        {...EMPTY}
        onClearFirmware={() => {}}
        onClearSchematic={() => {}}
      />,
    )

    expect(html).toContain('Additional evidence')
    expect(html).toContain('Accepted in this browser')
    expect(html).toContain('firmware-zone')
    expect(html).toContain('schematic-file')
    expect(html).toContain('BOM')
    expect(html).toContain('Placement / CPL')
    expect(html).toContain('Assembly variant')
    expect(html).toContain('As-built overlay')
    expect(html).toContain('Model library')
    expect(html).not.toContain('CLI-only inputs')
    expect(html).toContain('Datasheet model drafts')
  })

  test('does not offer file pickers for a restored report without files', () => {
    const html = renderToStaticMarkup(
      <AdditionalEvidencePanel
        placement="report"
        {...EMPTY}
        showWebControls={false}
      />,
    )

    expect(html).toContain('saved report has no files behind it')
    expect(html).not.toContain('firmware-zone')
    expect(html).not.toContain('schematic-file')
  })
})
