// The accepted-formats table as `/api/startup` serves it
// (`accepted_formats_json` in hauksbee-bind), in miniature. The frontend holds
// no list of its own, so a test of anything that reads one seeds this first.

import { setServedBoardFormats } from '../src/lib/board-formats'

export const SERVED_FORMATS = {
  formats: [
    { vendor: 'KiCad', exts: ['.kicad_pcb', '.kicad_sch', '.net'], route: 'board' as const, quiet: false },
    { vendor: 'ODB++ job', exts: ['.tgz', '.tar.gz', '.tar'], route: 'board' as const, quiet: false },
    { vendor: 'fab archive', exts: ['.zip'], route: 'board' as const, quiet: false },
    { vendor: 'SPICE deck', exts: ['.cir', '.sp', '.net'], route: 'deck' as const, quiet: false },
  ],
  firmware_exts: ['.elf', '.hex'],
}

export function seedServedFormats(): void {
  setServedBoardFormats(SERVED_FORMATS)
}
