//! One integration-test binary for the crate; each module was a
//! separate test file (and separate link step) before.

#[path = "altium.rs"]
mod altium;
#[path = "bom.rs"]
mod bom;
#[path = "drc.rs"]
mod drc;
#[path = "eagle_drc.rs"]
mod eagle_drc;
#[path = "eagle_tie_fixtures.rs"]
mod eagle_tie_fixtures;
#[path = "exchange_formats.rs"]
mod exchange_formats;
#[path = "extract.rs"]
mod extract;
#[path = "formats.rs"]
mod formats;
#[path = "gerber_advanced_geometry.rs"]
mod gerber_advanced_geometry;
#[path = "gerber_altium_metadata.rs"]
mod gerber_altium_metadata;
#[path = "gerber_gbrjob.rs"]
mod gerber_gbrjob;
#[path = "gerber_negative_pour.rs"]
mod gerber_negative_pour;
#[path = "gerber_order_determinism.rs"]
mod gerber_order_determinism;
#[path = "gerber_x2.rs"]
mod gerber_x2;
#[path = "gerber_x2_roles.rs"]
mod gerber_x2_roles;
#[path = "ingest_robustness.rs"]
mod ingest_robustness;
#[path = "kicad_dru.rs"]
mod kicad_dru;
#[path = "netlint.rs"]
mod netlint;
#[path = "netlist_code_missing.rs"]
mod netlist_code_missing;
#[path = "netlist_dnp.rs"]
mod netlist_dnp;
#[path = "reader_matrix.rs"]
mod reader_matrix;
#[path = "schematic.rs"]
mod schematic;
#[path = "subsheet_hierarchy.rs"]
mod subsheet_hierarchy;
