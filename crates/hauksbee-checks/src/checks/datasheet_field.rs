//! A part's `Datasheet` field that names a different device.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The field is copied from a symbol, a sibling part or a search result and is
//! often left pointing at the part it came from. Nothing else reads it back, so
//! the wrong link sits there until a model is drafted from the file it names
//! (`hauksbee models extract --unmodelled` reads exactly this field) and the
//! card describes another device.
//!
//! The judgement is [`hauksbee_extract::datasheet_identity`]'s, which is
//! deliberately conservative: only a link that ends in `.pdf` and carries a
//! part-number-like file name is read, document numbers and generic names say
//! nothing, and a family sheet that lists the part is silent. A finding is
//! `Severity::Low` and never gates; it asks the reader to look.

use std::collections::BTreeMap;

use hauksbee_bind::binder::component_mpn;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::datasheet_identity::datasheet_names_other_part;
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity, REMEDY_MARKER,
};

/// One component whose `Datasheet` field names another device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasheetDoubt {
    /// The part number the board gives (its MPN, else its value).
    pub part: String,
    /// The `Datasheet` field as written.
    pub locator: String,
    /// The file name the link ends in, without `.pdf`: what the link appears
    /// to be the datasheet for.
    pub names: String,
}

impl DatasheetDoubt {
    /// The one sentence both surfaces say, finishing with the choice the
    /// reader has: `models extract --unmodelled` appends "or confirm".
    #[must_use]
    pub fn sentence(&self) -> String {
        format!(
            "the board's datasheet for {} appears to be for {}",
            self.part, self.names
        )
    }
}

/// The field value of a component's `Datasheet` property, when it has one.
#[must_use]
pub fn datasheet_field(comp: &Component) -> Option<&str> {
    comp.properties
        .iter()
        .find(|(k, v)| k.eq_ignore_ascii_case("datasheet") && !v.trim().is_empty())
        .map(|(_, v)| v.trim())
}

/// The component's `Datasheet` field when it clearly names another device.
///
/// Every part number the component carries (each manufacturer-part-number
/// field, then its value) is tried: the link agrees when any of them does.
#[must_use]
pub fn component_doubt(comp: &Component) -> Option<DatasheetDoubt> {
    let locator = datasheet_field(comp)?;
    let mut identities: Vec<&str> = comp
        .properties
        .iter()
        .filter(|(k, v)| !v.trim().is_empty() && hauksbee_extract::bom::is_mpn_property_key(k))
        .map(|(_, v)| v.trim())
        .collect();
    identities.push(comp.value.trim());
    let names = datasheet_names_other_part(&identities, locator)?;
    Some(DatasheetDoubt {
        part: component_mpn(comp)?,
        locator: locator.to_string(),
        names,
    })
}

/// One low-severity finding per distinct (part, link) the board gets wrong,
/// naming every reference that carries it.
#[must_use]
pub fn datasheet_field_lint(board: &ExtractedBoard) -> NetLintReport {
    let mut by_link: BTreeMap<(String, String), (DatasheetDoubt, Vec<String>)> = BTreeMap::new();
    for comp in &board.components {
        if AssemblyState::of(comp).fitted().is_none() {
            continue;
        }
        if let Some(doubt) = component_doubt(comp) {
            by_link
                .entry((doubt.part.clone(), doubt.locator.clone()))
                .or_insert_with(|| (doubt, Vec::new()))
                .1
                .push(comp.reference.clone());
        }
    }
    let mut report = NetLintReport::default();
    for (doubt, mut refs) in by_link.into_values() {
        refs.sort_by_key(|r| hauksbee_ir::order::natural_ref_key(r));
        report.findings.push(LintFinding {
            check: LintCheck::DatasheetMismatch,
            severity: Severity::Low,
            message: format!(
                "{} has a Datasheet field that names a different device: {}, so a model \
                 drafted from it would describe the wrong part. {REMEDY_MARKER}replace the \
                 Datasheet field with the {} datasheet (for `hauksbee models extract \
                 --unmodelled`, pass --datasheet '{}=<correct pdf>').",
                refs.join(", "),
                doubt.sentence(),
                doubt.part,
                doubt.part
            ),
            refs,
            nets: Vec::new(),
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(reference: &str, value: &str, datasheet: &str) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            properties: vec![("Datasheet".into(), datasheet.into())],
            ..Default::default()
        }
    }

    fn lint(components: Vec<Component>) -> Vec<LintFinding> {
        datasheet_field_lint(&ExtractedBoard {
            name: "t".into(),
            nets: Vec::new(),
            components,
        })
        .findings
    }

    #[test]
    fn a_link_naming_another_device_is_one_low_finding_per_part_and_link() {
        let link = "https://www.ti.com/lit/ds/symlink/opa2336.pdf";
        let found = lint(vec![
            part("U7", "OPA379AIDCKR", link),
            part("U3", "OPA379AIDCKR", link),
            part(
                "U1",
                "74HC157D",
                "https://www.ti.com/lit/ds/symlink/sn74LS157.pdf",
            ),
        ]);
        assert_eq!(found.len(), 2, "{found:?}");
        let opa = found.iter().find(|f| f.refs.len() == 2).unwrap();
        assert_eq!(opa.refs, ["U3", "U7"]);
        assert_eq!(opa.severity, Severity::Low);
        assert_eq!(opa.check, LintCheck::DatasheetMismatch);
        assert!(
            opa.message
                .contains("the board's datasheet for OPA379AIDCKR appears to be for opa2336"),
            "{}",
            opa.message
        );
        assert!(opa
            .own_remedy()
            .unwrap()
            .contains("--datasheet 'OPA379AIDCKR="));
    }

    #[test]
    fn right_generic_and_absent_links_are_silent() {
        let found = lint(vec![
            part(
                "U1",
                "OPA379AIDCKR",
                "https://www.ti.com/lit/ds/symlink/opa379.pdf",
            ),
            part(
                "U2",
                "74HC4051D",
                "https://assets.nexperia.com/documents/74HC_HCT4051.pdf",
            ),
            part(
                "U3",
                "DMG1013T-7",
                "https://www.diodes.com/assets/Datasheets/ds31783.pdf",
            ),
            part("U4", "OPA379AIDCKR", "~"),
            part("U5", "OPA379AIDCKR", ""),
            part(
                "R1",
                "10k",
                "https://www.yageo.com/upload/PYu-RC_Group_51_RoHS_L_12.pdf",
            ),
        ]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_part_number_field_that_matches_silences_a_nickname_value() {
        let mut comp = part("U1", "AMP", "https://www.ti.com/lit/ds/symlink/opa379.pdf");
        comp.properties.push(("MPN".into(), "OPA379AIDCKR".into()));
        assert_eq!(component_doubt(&comp), None);
        let mut wrong = part("U2", "AMP", "https://www.ti.com/lit/ds/symlink/opa2336.pdf");
        wrong.properties.push(("MPN".into(), "OPA379AIDCKR".into()));
        let doubt = component_doubt(&wrong).unwrap();
        assert_eq!(
            (doubt.part.as_str(), doubt.names.as_str()),
            ("OPA379AIDCKR", "opa2336")
        );
    }
}
