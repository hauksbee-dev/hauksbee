Root schematics of Easyduino ESP32-S3 (https://github.com/Hanqaqa/Easyduino,
CERN-OHL-P-2.0, see UPSTREAM-LICENSE.txt) at v1.1 (`faulty`, parent
894c8566, UART TX wired to TX) and v1.2 (`fixed`, 4048171f), byte for byte.
The `References.kicad_sch` sub-sheet they name is left out on purpose:
`tests/cli_missing_subsheet.rs` checks a design whose sub-sheet is missing.
