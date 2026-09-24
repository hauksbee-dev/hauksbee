//! Orderable variants: sibling `[[models]]` entries that share `variant_of` and
//! the family match, told apart by `[models.variant]`. [`select`] scores every
//! sibling Satisfied, Contradicted or Unknown against the component at the
//! resolve tie point, so a board whose part name does not pin one orderable
//! variant is never silently simulated as whichever sibling sorts first.

use hauksbee_ir::evidence::Assumption;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{schema::ModelEntry, ComponentQuery};

/// Reserved component property holding an `[[override]] select` member id.
pub const SELECT_PROPERTY: &str = "hauksbee_select";
/// Reserved component property the CI member sweep sets for one run.
pub const SWEEP_PROPERTY: &str = "hauksbee_select_sweep";
/// Every variant-set assumption's `because` says this, so a report can find it.
pub const LISTED: &str = "variants the card lists";

/// `[models.variant]`. It has no `default` on purpose: which variant is fitted
/// is a fact about the board, asked of the user and never assumed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VariantSpec {
    /// Case-insensitive pattern for this member's orderable codes.
    pub orderable_re: String,
    /// The ordering-table codes, verbatim; the lint checks them against the siblings.
    #[serde(default)]
    pub orderable: Vec<String>,
    /// Package name, compared with the footprint's package family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Where the datasheet prints this variant.
    pub basis: String,
}

/// One member's verdict: satisfied, contradicted or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    S,
    C,
    U,
}

/// What settled the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Board,
    Spec,
    Sweep,
}

/// One sibling as the selection saw it.
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub id: String,
    /// The orderable codes (or the pattern) that pick it.
    pub selector: String,
    pub basis: String,
    pub verdict: Verdict,
    pub reason: String,
}

/// A variant set scored against one component.
#[derive(Debug, Clone, Serialize)]
pub struct Selection {
    pub family: String,
    pub members: Vec<Member>,
    /// The one member left once contradicted members are dropped.
    pub determined: Option<String>,
    pub via: Via,
    /// Every member maps the same pads to the same roles.
    pub same_pinout: bool,
}

impl Selection {
    pub fn survivors(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| m.verdict != Verdict::C)
    }

    /// The member to bind: the determined one, else for a same-pinout set the
    /// first survivor as a flagged stand-in, else none (the part stays open).
    pub fn bound(&self) -> Option<&str> {
        let stand_in = || self.survivors().next().filter(|_| self.same_pinout);
        self.determined.as_deref().or_else(|| stand_in().map(|m| m.id.as_str()))
    }

    /// The spec line that settles the set for `reference`.
    pub fn override_line(&self, reference: &str) -> String {
        let id = self.survivors().next().map_or("<member id>", |m| &m.id);
        format!("[[override]] ref=\"{reference}\" select=\"{id}\"")
    }

    /// Every member with its selector, source and verdict, for a message.
    pub fn listing(&self) -> String {
        let line = |m: &Member| {
            let v = m.verdict;
            format!("{} ({}; {}): {v:?}, {}", m.id, m.selector, m.basis, m.reason)
        };
        self.members.iter().map(line).collect::<Vec<_>>().join("; ")
    }

    /// Why a set with nothing to bind leaves `reference` open, and what closes it.
    pub fn open_reason(&self, reference: &str) -> String {
        let (n, family, unlock) = (self.members.len(), &self.family, Assumption::UNLOCKED_BY_MARKER);
        let (line, listing) = (self.override_line(reference), self.listing());
        if self.survivors().next().is_none() {
            return format!(
                "the board fits none of the {n} {family} variants the card lists: {listing}.{unlock}\
                 fix the part number or footprint, or add `{line}` naming the fitted variant"
            );
        }
        format!(
            "selection required for {reference}: the {n} {family} variants the card lists differ \
             in pinout and nothing on the board picks one: {listing}.{unlock}add `{line}` to the \
             spec, or put the orderable code in the BOM"
        )
    }
}

/// The package family a package or footprint name spells (`SOT` for `SOT-23-5`).
pub fn package_family(package: &str) -> Option<&'static str> {
    let package: String = package.chars().filter(char::is_ascii_alphanumeric).collect();
    let package = package.to_ascii_uppercase();
    ["TSSOP", "SSOP", "SOIC", "TQFP", "LQFP", "QFN", "DFN", "BGA", "DIP", "SOT"]
        .into_iter()
        .find(|family| package.contains(family))
}

fn orderable_re(entry: &ModelEntry) -> Option<Regex> {
    Regex::new(&format!("(?i){}", entry.variant.as_ref()?.orderable_re)).ok()
}

/// Score `siblings` (one `variant_of` family, sorted by id) against `q`.
pub fn select(family: &str, siblings: &[&ModelEntry], q: &ComponentQuery) -> Selection {
    let property = |key: &str| q.properties.iter().rev().find(|(k, _)| k == key);
    let (picked, via) = match (property(SELECT_PROPERTY), property(SWEEP_PROPERTY)) {
        (Some((_, id)), _) => (Some(id.trim()), Via::Spec),
        (None, Some((_, id))) => (Some(id.trim()), Via::Sweep),
        (None, None) => (None, Via::Board),
    };
    let patterns: Vec<Option<Regex>> = siblings.iter().map(|e| orderable_re(e)).collect();
    // The first identity string (MPN, value, then properties) any member's code matches.
    let reserved = |k: &str| k == SELECT_PROPERTY || k == SWEEP_PROPERTY;
    let texts = [q.mpn.as_deref(), q.value.as_deref()].into_iter().flatten();
    let texts = texts.chain(q.properties.iter().filter(|(k, _)| !reserved(k)).map(|(_, v)| v.as_str()));
    let identity = texts.map(str::trim).find_map(|text| {
        let hits: Vec<usize> = (0..siblings.len())
            .filter(|&i| patterns[i].as_ref().is_some_and(|re| re.is_match(text)))
            .collect();
        (!hits.is_empty()).then_some((text, hits))
    });
    let footprint = q.footprint.as_deref().map(|f| f.rsplit(':').next().unwrap_or(f));
    let members: Vec<Member> = siblings.iter().enumerate().map(|(i, e)| {
        let v = e.variant.clone().unwrap_or_default();
        let (mut sat, mut con) = (Vec::new(), Vec::new());
        let who = if via == Via::Spec { "the spec" } else { "this sweep run" };
        match picked {
            Some(id) if id == e.id => sat.push(format!("{who} selects it")),
            Some(id) => con.push(format!("{who} selects {id}")),
            None => {}
        }
        match &identity {
            Some((text, hits)) if !hits.contains(&i) => con.push(format!("{text} is another variant's code")),
            Some((text, hits)) if hits.len() == 1 => sat.push(format!("{text} is its orderable code")),
            _ => {}
        }
        let package = v.package.as_deref().zip(footprint);
        match package.and_then(|(p, f)| Some((p, package_family(p)?, package_family(f)?))) {
            Some((p, want, got)) if want != got => con.push(format!("package {p} is not a {got} footprint")),
            Some((p, ..)) => sat.push(format!("package {p} fits the footprint")),
            None => {}
        }
        let absent = e.pins.keys().find(|pad| !q.pads.is_empty() && !q.pads.contains(pad));
        con.extend(absent.map(|pad| format!("its pad {pad} is not on the footprint")));
        let (verdict, reasons) = match (con.is_empty(), sat.is_empty()) {
            (false, _) => (Verdict::C, con),
            (true, false) => (Verdict::S, sat),
            (true, true) => (Verdict::U, vec!["nothing on the board names it".to_string()]),
        };
        let selector = if v.orderable.is_empty() { v.orderable_re } else { v.orderable.join(", ") };
        Member { id: e.id.clone(), selector, basis: v.basis, verdict, reason: reasons.join("; ") }
    }).collect();
    let mut left = members.iter().filter(|m| m.verdict != Verdict::C);
    let determined = match (left.next(), left.next()) {
        (Some(only), None) => Some(only.id.clone()),
        _ => None,
    };
    let same_pinout = siblings.windows(2).all(|pair| pair[0].pins == pair[1].pins);
    Selection { family: family.to_string(), members, determined, via, same_pinout }
}

/// Variant-set findings over the entries of one file: a family has two or more
/// members sharing `variant_of` and the family match, each with a
/// `[models.variant]`, and every orderable code matches its own `orderable_re`
/// and the family match and no sibling's.
pub fn lint(entries: &[ModelEntry]) -> Vec<String> {
    let mut out = Vec::new();
    for e in entries.iter().filter(|e| e.variant.is_some() != e.variant_of.is_some()) {
        out.push(format!("model '{}': variant_of and [models.variant] come together", e.id));
    }
    let families: std::collections::BTreeSet<&str> =
        entries.iter().filter_map(|e| e.variant_of.as_deref()).collect();
    for family in families {
        let siblings: Vec<&ModelEntry> =
            entries.iter().filter(|e| e.variant_of.as_deref() == Some(family)).collect();
        if siblings.len() < 2 || siblings.iter().any(|e| e.r#match != siblings[0].r#match) {
            out.push(format!(
                "variant_of '{family}': a variant set is two or more entries sharing one family match"
            ));
        }
        let family_re = siblings[0].r#match.value_re.as_ref().or(siblings[0].r#match.mpn_re.as_ref());
        let family_re = family_re.and_then(|p| Regex::new(&format!("(?i){p}")).ok());
        let patterns: Vec<Option<Regex>> = siblings.iter().map(|e| orderable_re(e)).collect();
        for (i, e) in siblings.iter().enumerate() {
            let codes = e.variant.as_ref().map_or(&[][..], |v| &v.orderable[..]);
            if patterns[i].is_none() || codes.is_empty() {
                out.push(format!("model '{}': [models.variant] needs a valid orderable_re and its ordering-table codes in `orderable`", e.id));
            }
            for code in codes {
                let hit = |re: &Option<Regex>| re.as_ref().is_some_and(|re| re.is_match(code));
                let others = (0..siblings.len()).filter(|&j| j != i && hit(&patterns[j]));
                let others: Vec<&str> = others.map(|j| siblings[j].id.as_str()).collect();
                if !hit(&patterns[i]) || !hit(&family_re) || !others.is_empty() {
                    out.push(format!(
                        "model '{}': orderable code {code} must match its own orderable_re and the family match and no sibling's (also matches: {})",
                        e.id,
                        others.join(", ")
                    ));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelLibrary, SourceLayer};

    /// A synthetic two-member family: `b_pins`/`b_package` say how B differs from A.
    fn family(b_pins: &str, b_package: &str) -> String {
        let member = |id: &str, vout: f64, pins: &str, package: &str| {
            format!(
                "[[models]]\nid = \"xyz1234{id}\"\nkind = \"vreg\"\nvariant_of = \"XYZ1234\"\n\
                 [models.match]\nvalue_re = \"(?i)^XYZ1234\"\n[models.params]\nvout = {vout}\ndropout_v = 0.3\niq_a = 1e-4\n\
                 [models.pins]\n{pins}\n[models.variant]\norderable_re = \"^XYZ1234{up}\"\n\
                 orderable = [\"XYZ1234{up}DBV\", \"XYZ1234{up}DBVR\"]\npackage = \"{package}\"\n\
                 basis = \"p. 3, Ordering Information\"\n",
                up = id.to_uppercase()
            )
        };
        member("a", 3.3, SAME, "SOT-23") + &member("b", 5.0, b_pins, b_package)
    }

    const SAME: &str = "\"1\" = \"in\"\n\"2\" = \"gnd\"\n\"3\" = \"out\"";
    const OTHER: &str = "\"1\" = \"gnd\"\n\"2\" = \"out\"\n\"3\" = \"in\"\n\"4\" = \"out\"";

    fn load(src: &str) -> Result<ModelLibrary, String> {
        let mut lib = ModelLibrary::empty();
        let loaded = lib.load_toml_str(src, "xyz.toml", SourceLayer::UserDir);
        loaded.map(|()| lib).map_err(|e| e.to_string())
    }

    fn query(value: &str, mpn: Option<&str>, footprint: Option<&str>, select: Option<&str>) -> ComponentQuery {
        let properties = select.map(|id| (SELECT_PROPERTY.to_string(), id.to_string()));
        ComponentQuery {
            value: Some(value.into()),
            mpn: mpn.map(Into::into),
            footprint: footprint.map(Into::into),
            properties: properties.into_iter().collect(),
            pads: ["1", "2", "3"].map(String::from).to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn selection_binds_what_the_board_determines_and_never_guesses_a_pinout() {
        type Case<'a> = (&'a str, &'a str, &'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>, Option<&'a str>, Via);
        let (soic, sot) = (Some("Package_SO:SOIC-8"), Some("Package_TO_SOT_SMD:SOT-23"));
        let cases: [Case; 9] = [
            // (case, B pins, value, mpn, footprint, select) -> determined, via
            ("value names A's code", OTHER, "XYZ1234ADBVR", None, None, None, Some("xyz1234a"), Via::Board),
            ("mpn names B's code", SAME, "XYZ1234", Some("XYZ1234BDBV"), None, None, Some("xyz1234b"), Via::Board),
            ("package picks B", SAME, "XYZ1234", None, soic, None, Some("xyz1234b"), Via::Board),
            ("pads rule out B's pad 4", OTHER, "XYZ1234", None, None, None, Some("xyz1234a"), Via::Board),
            ("spec selects B", SAME, "XYZ1234", None, None, Some("xyz1234b"), Some("xyz1234b"), Via::Spec),
            ("same pinout, undetermined", SAME, "XYZ1234", None, None, None, None, Via::Board),
            ("select contradicts the mpn", SAME, "XYZ1234", Some("XYZ1234ADBV"), None, Some("xyz1234b"), None, Via::Spec),
            ("package contradicts the mpn", SAME, "XYZ1234", Some("XYZ1234BDBV"), sot, None, None, Via::Board),
            ("unknown select", SAME, "XYZ1234", None, None, Some("xyz1234z"), None, Via::Spec),
        ];
        for (case, b_pins, value, mpn, footprint, select, determined, via) in cases {
            let lib = load(&family(b_pins, "SOIC-8")).unwrap();
            let res = lib.resolve(&query(value, mpn, footprint, select));
            let selection = res.selection.as_ref().expect(case);
            assert_eq!(selection.determined.as_deref(), determined, "{case}: {}", selection.listing());
            assert_eq!(selection.via, via, "{case}");
            let bound = res.model.as_ref().map(|m| m.id.as_str());
            assert_eq!(bound, selection.bound(), "{case}");
            assert!(res.tied.is_empty(), "{case}: siblings are not a tie");
        }
    }

    #[test]
    fn an_undetermined_set_stands_in_on_a_shared_pinout_and_stays_open_otherwise() {
        let lib = load(&family(SAME, "SOT-23")).unwrap();
        let res = lib.resolve(&query("XYZ1234", None, None, None));
        assert_eq!(res.model.map(|m| m.id).as_deref(), Some("xyz1234a"));

        let q = ComponentQuery { pads: Vec::new(), ..query("XYZ1234", None, None, None) };
        let res = load(&family(OTHER, "SOT-23")).unwrap().resolve(&q);
        assert!(res.model.is_none());
        let reason = res.selection.expect("a variant set").open_reason("U3");
        let wants = [
            "selection required for U3",
            "xyz1234a (XYZ1234ADBV, XYZ1234ADBVR; p. 3",
            "[[override]] ref=\"U3\" select=\"xyz1234a\"",
            Assumption::UNLOCKED_BY_MARKER,
        ];
        for want in wants {
            assert!(reason.contains(want), "{want} in {reason}");
        }

        let q = query("XYZ1234", Some("XYZ1234ADBV"), None, Some("xyz1234b"));
        let reason = lib.resolve(&q).selection.expect("a variant set").open_reason("U3");
        assert!(reason.starts_with("the board fits none of the 2 XYZ1234 variants"), "{reason}");
    }

    #[test]
    fn the_lint_rejects_malformed_variant_sets() {
        let good = family(SAME, "SOT-23");
        assert!(lint_src(&good).is_empty(), "{:?}", lint_src(&good));
        let lone = good.split("[[models]]").nth(1).map(|a| format!("[[models]]{a}")).unwrap();
        let other_match = "^XYZ12\"\n[models.params]\nvout = 5";
        let cases = [
            ("no [models.variant]", good.replacen("[models.variant]", "[models.variantx]", 1), "come together"),
            ("lone member", lone, "two or more entries"),
            ("shared code", good.replacen("^XYZ1234B", "^XYZ1234", 1), "no sibling's (also matches: xyz1234b)"),
            ("outside the family", good.replacen("XYZ1234ADBVR", "ABC1234ADBVR", 1), "orderable code ABC1234ADBVR"),
            ("no codes", good.replacen("[\"XYZ1234ADBV\", \"XYZ1234ADBVR\"]", "[]", 1), "ordering-table codes"),
            ("different match", good.replacen("^XYZ1234\"\n[models.params]\nvout = 5", other_match, 1), "one family match"),
        ];
        for (case, src, want) in cases {
            let found = lint_src(&src).join("\n");
            assert!(found.contains(want), "{case}: {found}");
            assert!(load(&src).is_err(), "{case} loads");
        }
        let defaulted = good.replacen("basis = ", "default = true\nbasis = ", 1);
        assert!(load(&defaulted).err().unwrap_or_default().contains("default"), "variants carry no default");
    }

    fn lint_src(src: &str) -> Vec<String> {
        toml::from_str::<crate::schema::DbFile>(src).map_or_else(|e| vec![e.to_string()], |file| lint(&file.models))
    }
}
