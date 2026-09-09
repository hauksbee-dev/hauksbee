import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { AdditionalEvidencePanel } from '../src/components/AdditionalEvidencePanel'

describe('additional evidence surface', () => {
  test('offers the complete supplemental-input contract in the browser', () => {
    const html = renderToStaticMarkup(
      <AdditionalEvidencePanel
        placement="intake"
        firmware={null}
        schematic={null}
        bom={null}
        placementFile={null}
        variant={null}
        asbuilt={null}
        models={[]}
        onFirmware={() => {}}
        onClearFirmware={() => {}}
        onSchematic={() => {}}
        onClearSchematic={() => {}}
        onBom={() => {}}
        onPlacement={() => {}}
        onVariant={() => {}}
        onAsbuilt={() => {}}
        onModels={() => {}}
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
        firmware={null}
        schematic={null}
        bom={null}
        placementFile={null}
        variant={null}
        asbuilt={null}
        models={[]}
        onFirmware={() => {}}
        onSchematic={() => {}}
        onBom={() => {}}
        onPlacement={() => {}}
        onVariant={() => {}}
        onAsbuilt={() => {}}
        onModels={() => {}}
        showWebControls={false}
      />,
    )

    expect(html).toContain('saved report has no files behind it')
    expect(html).not.toContain('firmware-zone')
    expect(html).not.toContain('schematic-file')
  })
})
