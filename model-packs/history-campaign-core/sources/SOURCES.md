# Sources and scope

This pack is a deliberately small, source-bound coverage increment for the
frozen history campaign. The part/value strings below are the exact values in
the pinned campaign boards, and the pin maps are copied from the cited primary
datasheet pin tables. A local hash is recorded for the copies inspected while
authoring; the PDFs are not redistributed by the pack.

| model value | primary source | inspected-copy SHA-256 | scope |
| --- | --- | --- | --- |
| `NCP1117-3.3_SOT223` | onsemi NCP1117/NCP1117I/NCV1117, NCP1117/D Rev. 31 (Aug 2021), [PDF](https://www.onsemi.com/pdf/datasheet/ncp1117-d.pdf), Pin Configuration/Table 1 | `c4989ad7fef42829443273b0c87067450e66a78c68bb6c6fd73aa6ec80a36051` | fixed-output LDO value, pin map, nominal output; no transient/current-limit law |
| `AP64501SP-13` | Diodes Incorporated AP64501 DS41980 Rev. 5 (Dec 2024), [PDF](https://www.diodes.com/assets/Datasheets/AP64501.pdf), pp. 1/3 | `71e2d1e8b53137e82fac54fc23a79a96882cc2c320c2227b783f66f5abf81d65` | identity and pin map only; no switching, feedback, inductor, or protection behavior |
| `74AHCT1G32SE-7` | Diodes Incorporated 74AHCT1G32 DS35184 Rev. 1-2 (May 2011), [PDF](https://www.diodes.com/assets/Datasheets/74AHCT1G32.pdf), pp. 1-3 | `5bec1094207164bd6109a21a3c7e3ddb61265b5c1fe7102584daa565bdfc1b58` | 2-input positive-OR logic, levels and pin map; no package parasitics |
| `BMP280` | Bosch Sensortec BMP280 BST-BMP280-DS001-26 Rev. 1.26 (Oct 2021), [PDF](https://www.bosch-sensortec.com/media/boschsensortec/downloads/datasheets/bst-bmp280-ds001.pdf), Table 29 | `473ff27d9df698b4757e36b36209f83b9f637b592c999d5fabe2a9453a488da6` | identity, supply/logic levels and bus pin map; no register map or measurement response |
| `SN65HVD230` | TI SN65HVD20/21/22/23/24 SLLS552G (Sep 2022), [PDF](https://www.ti.com/lit/ds/symlink/sn65hvd23.pdf), Table 7-1 | `7a91efb110506aa4da5745cfd10fb8a6b9bd3270d25227f71ae8cb33c89df823` | CAN transceiver identity and pin map; no CAN electrical/driver state machine |
| `TLP2761` | Toshiba TLP2761 Rev. 9.0 (Dec 2017 copy), [PDF](https://toshiba.semicon-storage.com/info/TLP2761_datasheet_en_20171226.pdf?did=28819&prodName=TLP2761), Pin Assignment | not retained; source URL and pin-table fact are retained | identity and pin map only; optocoupler transfer and output timing are not modeled |

Validation tier is intentionally `physical-bounds-only` for identity-only cards;
`datasheet-curves` is used only for the fixed NCP1117 nominal output and the
74AHCT1G32 logic levels. These are model-source claims, not measured-board
validation and not root-cause closure for any issue.

## Frozen-campaign denominator

The current frozen artifacts contain 61 `active_ic` occurrences across 45 unique
value strings (49 unique reference/value tuples; paired controls duplicate some
values). This first pack supplies source-bound identity/pin facts for 7
references in 3 exact `input-fix.kicad_pcb` artifacts. Only 3 of those
references receive executable behaviour; the AP64501, BMP280, SN65HVD230, and
TLP2761 entries are explicitly identity-only and remain OPEN. It is a curated
subset, not a claim to close the full unresolved backlog or the earlier
informal 48-item summary.
