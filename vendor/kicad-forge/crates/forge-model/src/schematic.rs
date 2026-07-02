//! Minimal typed views for `.kicad_sch` files.
//!
//! Covers enough to extract symbol instances (lib_id, reference, value, uuid)
//! and sheet hierarchy for netlist extraction.

use forge_sexpr::{parse, List};

use crate::Error;

/// A parsed KiCad schematic file.
pub struct Schematic {
    doc: forge_sexpr::Document,
}

impl Schematic {
    pub fn parse(text: &str) -> Result<Schematic, Error> {
        let doc = parse(text)?;
        let root_name = doc
            .root()
            .and_then(|l| l.name())
            .unwrap_or("")
            .to_string();
        if root_name != "kicad_sch" {
            return Err(Error::NotSchematic(root_name));
        }
        Ok(Schematic { doc })
    }

    pub fn emit(&self) -> String {
        self.doc.emit()
    }

    fn root(&self) -> &List {
        self.doc.root().expect("verified root exists")
    }

    /// All top-level `(symbol ...)` instances (not lib_symbols definitions).
    pub fn symbols(&self) -> Vec<SchematicSymbol<'_>> {
        self.root()
            .find_all("symbol")
            .filter(|l| {
                // Symbol instances have `(lib_id ...)` child; lib definitions
                // inside `(lib_symbols ...)` don't appear at root level so
                // this filter is just belt-and-suspenders.
                l.find("lib_id").is_some()
            })
            .map(|l| SchematicSymbol { list: l })
            .collect()
    }

    /// Sub-sheet references in the hierarchy (`(sheet ...)`).
    pub fn sheets(&self) -> Vec<SchematicSheet<'_>> {
        self.root()
            .find_all("sheet")
            .map(|l| SchematicSheet { list: l })
            .collect()
    }

    pub fn version(&self) -> i64 {
        self.root().find_i64("version").unwrap_or(0)
    }
}

/// A symbol instance in a schematic.
pub struct SchematicSymbol<'a> {
    list: &'a List,
}

impl<'a> SchematicSymbol<'a> {
    pub fn lib_id(&self) -> String {
        self.list.find_value("lib_id").unwrap_or_default()
    }

    pub fn reference(&self) -> Option<String> {
        self.property("Reference")
    }

    pub fn value(&self) -> Option<String> {
        self.property("Value")
    }

    pub fn uuid(&self) -> Option<String> {
        self.list.find_value("uuid")
    }

    pub fn property(&self, key: &str) -> Option<String> {
        for l in self.list.find_all("property") {
            if l.arg_value(0).as_deref() == Some(key) {
                return l.arg_value(1);
            }
        }
        None
    }
}

/// A sub-sheet reference.
pub struct SchematicSheet<'a> {
    list: &'a List,
}

impl<'a> SchematicSheet<'a> {
    pub fn uuid(&self) -> Option<String> {
        self.list.find_value("uuid")
    }

    pub fn sheet_name(&self) -> Option<String> {
        for l in self.list.find_all("property") {
            if l.arg_value(0).as_deref() == Some("Sheetname") {
                return l.arg_value(1);
            }
        }
        None
    }

    pub fn sheet_file(&self) -> Option<String> {
        for l in self.list.find_all("property") {
            if l.arg_value(0).as_deref() == Some("Sheetfile") {
                return l.arg_value(1);
            }
        }
        None
    }
}
