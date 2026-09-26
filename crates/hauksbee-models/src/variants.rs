//! Orderable variants: sibling `[[models]]` entries that share `variant_of` and
//! the family match, told apart by `[models.variant]`. [`select`] scores each
//! sibling against the component at the resolve tie point, so a part name that
//! pins no variant is never silently simulated as whichever sibling sorts first.

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

/// `[models.variant]`. It has no `default`: which variant is fitted is a fact
/// about the board, asked of the user and never assumed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VariantSpec {
    /// Case-insensitive pattern for this member's orderable codes.
    pub orderable_re: String,
    /// The ordering-table codes, verbatim.
    #[serde(default)]
    pub orderable: Vec<String>,
    /// Package name, compared with the footprint's package family.
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

/// One sibling as the selection saw it; `selector` is its orderable codes.
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub id: String,
    pub selector: String,
    pub basis: String,
    pub verdict: Verdict,
    pub reason: String,
}

/// A variant set scored against one component (`reference`, `value`).
/// `determined` is the one member left once contradicted ones drop;
/// `same_pinout` says every member maps the same pads to the same roles.
#[derive(Debug, Clone, Serialize)]
pub struct Selection {
    pub reference: String,
    pub value: String,
    pub family: String,
    pub members: Vec<Member>,
    pub determined: Option<String>,
    pub via: Via,
    pub same_pinout: bool,
}

impl Selection {
    pub fn survivors(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| m.verdict != Verdict::C)
    }

    /// The member to bind: the determined one, else for a same-pinout set the
    /// first survivor as a flagged stand-in, else none (the part stays open).
    #[must_use]
    pub fn bound(&self) -> Option<&str> {
        let stand_in = self.survivors().find(|_| self.same_pinout);
        self.determined
            .as_deref()
            .or_else(|| stand_in.map(|m| &*m.id))
    }

    /// The spec line that settles the set for `reference`.
    #[must_use]
    pub fn override_line(&self, reference: &str) -> String {
        let id = self.survivors().next().map_or("<member id>", |m| &m.id);
        format!("[[override]] ref=\"{reference}\" select=\"{id}\"")
    }

    /// The member ids, for a sentence.
    #[must_use]
    pub fn ids(&self) -> String {
        let ids: Vec<&str> = self.members.iter().map(|m| &*m.id).collect();
        ids.join(", ")
    }

    /// Every member with its selector, source and verdict, for a message.
    #[must_use]
    pub fn listing(&self) -> String {
        let head = |m: &Member| format!("{} ({}; {})", m.id, m.selector, m.basis);
        let line = |m: &Member| format!("{}: {:?}, {}", head(m), m.verdict, m.reason);
        self.members.iter().map(line).collect::<Vec<_>>().join("; ")
    }

    /// Why a set with nothing to bind leaves `reference` open, and what closes it.
    #[must_use]
    pub fn open_reason(&self, reference: &str) -> String {
        let (n, family, listing) = (self.members.len(), &self.family, self.listing());
        let unlock = Assumption::UNLOCKED_BY_MARKER;
        let line = self.override_line(reference);
        if self.survivors().next().is_none() {
            return format!(
                "the board fits none of the {n} {family} {LISTED}: {listing}.{unlock}fix the part \
                 number or footprint, or add `{line}` naming the fitted variant"
            );
        }
        format!(
            "selection required for {reference}: the {n} {family} {LISTED} differ in pinout and \
             nothing on the board picks one: {listing}.{unlock}add `{line}` to the spec, or put \
             the orderable code in the BOM"
        )
    }
}

/// The package family a package or footprint name spells (`SOT` for `SOT-23-5`).
#[must_use]
pub fn package_family(package: &str) -> Option<&'static str> {
    let upper = package.to_ascii_uppercase();
    let squashed = upper.replace(|c: char| !c.is_ascii_alphanumeric(), "");
    let families = ["TSSOP", "SSOP", "SOIC", "TQFP", "LQFP"];
    let mut families = families
        .into_iter()
        .chain(["QFN", "DFN", "BGA", "DIP", "SOT"]);
    families.find(|f| squashed.contains(f))
}

fn orderable_re(entry: &ModelEntry) -> Option<Regex> {
    Regex::new(&format!("(?i){}", entry.variant.as_ref()?.orderable_re)).ok()
}

/// Score `siblings` (one `variant_of` family, sorted by id) against `q`: each
/// fact the board states about a member holds or contradicts it.
pub fn select(family: &str, siblings: &[&ModelEntry], q: &ComponentQuery) -> Selection {
    let property = |key: &str| q.properties.iter().rev().find(|(k, _)| k == key);
    let (picked, via, who) = match (property(SELECT_PROPERTY), property(SWEEP_PROPERTY)) {
        (Some((_, id)), _) => (Some(id.trim()), Via::Spec, "the spec"),
        (None, Some((_, id))) => (Some(id.trim()), Via::Sweep, "this sweep run"),
        (None, None) => (None, Via::Board, ""),
    };
    let patterns: Vec<Option<Regex>> = siblings.iter().map(|e| orderable_re(e)).collect();
    let hit = |i: usize, text: &str| patterns[i].as_ref().is_some_and(|re| re.is_match(text));
    // The first identity text (MPN, value, then properties) some member's code matches.
    let own = |(k, _): &&(String, String)| k != SELECT_PROPERTY && k != SWEEP_PROPERTY;
    let props = q.properties.iter().filter(own).map(|(_, v)| v.as_str());
    let texts = [q.mpn.as_deref(), q.value.as_deref()];
    let mut texts = texts.into_iter().flatten().chain(props).map(str::trim);
    let identity = texts.find(|text| (0..siblings.len()).any(|i| hit(i, text)));
    let footprint = q.footprint.as_deref().and_then(|f| f.rsplit(':').next());
    let missing = |pad: &&String| !q.pads.is_empty() && !q.pads.contains(pad);
    let score = |(i, e): (usize, &&ModelEntry)| {
        let v = e.variant.clone().unwrap_or_default();
        let mut facts: Vec<(bool, String)> = Vec::new();
        facts.extend(picked.map(|id| (id == e.id, format!("{who} selects {id}"))));
        facts.extend(identity.map(|text| (hit(i, text), format!("code {text}"))));
        let package = v.package.as_deref().zip(footprint);
        let package = package.and_then(|(p, f)| Some((p, package_family(p)?, package_family(f)?)));
        let fits = |(p, want, got)| (want == got, format!("package {p}, {got} footprint"));
        facts.extend(package.map(fits));
        let absent = e.pins.keys().find(missing);
        facts.extend(absent.map(|pad| (false, format!("no pad {pad} on the footprint"))));
        let (held, broken): (Vec<_>, Vec<_>) = facts.into_iter().partition(|f| f.0);
        let (verdict, shown) = match (broken.is_empty(), held.is_empty()) {
            (false, _) => (Verdict::C, broken),
            (true, false) => (Verdict::S, held),
            (true, true) => (Verdict::U, vec![(true, "nothing names it".into())]),
        };
        let reason: Vec<String> = shown.into_iter().map(|f| f.1).collect();
        Member {
            id: e.id.clone(),
            selector: v.orderable.join(", "),
            basis: v.basis,
            verdict,
            reason: reason.join("; "),
        }
    };
    let members: Vec<Member> = siblings.iter().enumerate().map(score).collect();
    let left: Vec<&Member> = members.iter().filter(|m| m.verdict != Verdict::C).collect();
    let determined = (left.len() == 1).then(|| left[0].id.clone());
    let same_pinout = siblings.windows(2).all(|pair| pair[0].pins == pair[1].pins);
    let text = |t: &Option<String>| t.clone().unwrap_or_default();
    Selection {
        reference: text(&q.reference),
        value: text(&q.value),
        family: family.to_string(),
        members,
        determined,
        via,
        same_pinout,
    }
}

/// Variant-set findings over the entries of one file; the rules are the messages below.
#[must_use]
pub fn lint(entries: &[ModelEntry]) -> Vec<String> {
    let mut out = Vec::new();
    let mut families = std::collections::BTreeMap::<&str, Vec<&ModelEntry>>::new();
    for e in entries {
        match (&e.variant_of, &e.variant) {
            (Some(family), Some(_)) => families.entry(family).or_default().push(e),
            (None, None) => {}
            _ => out.push(format!("model '{}': {TOGETHER}", e.id)),
        }
    }
    for (family, siblings) in families {
        let first = &siblings[0].r#match;
        if siblings.len() < 2 || siblings.iter().any(|e| e.r#match != *first) {
            out.push(format!("variant_of '{family}': {SET}"));
        }
        let family_re = first.value_re.as_ref().or(first.mpn_re.as_ref());
        let family_re = family_re.and_then(|p| Regex::new(&format!("(?i){p}")).ok());
        let patterns: Vec<Option<Regex>> = siblings.iter().map(|e| orderable_re(e)).collect();
        for (i, e) in siblings.iter().enumerate() {
            let codes = e.variant.as_ref().map_or(&[][..], |v| &v.orderable[..]);
            if patterns[i].is_none() || codes.is_empty() {
                out.push(format!("model '{}': {CODES}", e.id));
            }
            for code in codes {
                let hit = |re: &Option<Regex>| re.as_ref().is_some_and(|re| re.is_match(code));
                let others = (0..siblings.len()).filter(|&j| j != i && hit(&patterns[j]));
                let others: Vec<&str> = others.map(|j| siblings[j].id.as_str()).collect();
                if !hit(&patterns[i]) || !hit(&family_re) || !others.is_empty() {
                    let (id, also) = (&e.id, others.join(", "));
                    out.push(format!("model '{id}': orderable code {code} {OWN} {also})"));
                }
            }
        }
    }
    out
}

const TOGETHER: &str = "variant_of and [models.variant] come together";
const SET: &str = "a variant set is two or more entries sharing one family match";
const CODES: &str =
    "[models.variant] needs a valid orderable_re and its ordering-table codes in `orderable`";
const OWN: &str =
    "must match its own orderable_re and the family match and no sibling's (also matches:";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelLibrary, SourceLayer};

    const SAME: &str = "\"1\" = \"in\"\n\"2\" = \"gnd\"\n\"3\" = \"out\"";
    const OTHER: &str = "\"1\" = \"gnd\"\n\"2\" = \"out\"\n\"3\" = \"in\"\n\"4\" = \"out\"";

    /// A synthetic two-member family; B's pinout and package are the knobs.
    fn family(b_pins: &str, b_package: &str) -> String {
        let member = |id: char, pins: &str, package: &str| {
            format!(
                "[[models]]\nid = \"xyz1234{id}\"\nkind = \"vreg\"\nvariant_of = \"XYZ1234\"\n\
                 [models.match]\nvalue_re = \"(?i)^XYZ1234\"\n\
                 [models.params]\nvout = 3.3\ndropout_v = 0.3\niq_a = 1e-4\n[models.pins]\n{pins}\n\
                 [models.variant]\norderable_re = \"^XYZ1234{id}\"\npackage = \"{package}\"\n\
                 orderable = [\"XYZ1234{id}DBV\", \"XYZ1234{id}DBVR\"]\nbasis = \"p. 3\"\n"
            )
        };
        member('a', SAME, "SOT-23") + &member('b', b_pins, b_package)
    }

    fn load(src: &str) -> Result<ModelLibrary, String> {
        let mut lib = ModelLibrary::empty();
        let loaded = lib.load_toml_str(src, "xyz.toml", SourceLayer::UserDir);
        loaded.map(|()| lib).map_err(|e| e.to_string())
    }

    #[test]
    fn selection_binds_what_the_board_determines_and_never_guesses_a_pinout() {
        let (soic, sot) = (Some("Package_SO:SOIC-8"), Some("Package_TO_SOT_SMD:SOT-23"));
        let (pads, none) = (["1", "2", "3"].map(String::from).to_vec(), &vec![]);
        #[rustfmt::skip]
        let cases = [
            // (case, B pins, value, mpn, footprint, select, pads) -> bound, determined, via, text
            ("value names A's code", OTHER, "XYZ1234ADBVR", None, None, None, &pads, Some("a"), true, Via::Board, "S, code XYZ1234ADBVR"),
            ("mpn names B's code", SAME, "XYZ1234", Some("XYZ1234BDBV"), None, None, &pads, Some("b"), true, Via::Board, "C, code XYZ1234BDBV"),
            ("package picks B", SAME, "XYZ1234", None, soic, None, &pads, Some("b"), true, Via::Board, "C, package SOT-23, SOIC footprint"),
            ("pads rule out B's pad 4", OTHER, "XYZ1234", None, None, None, &pads, Some("a"), true, Via::Board, "C, no pad 4 on the footprint"),
            ("spec selects B", OTHER, "XYZ1234", None, None, Some("xyz1234b"), none, Some("b"), true, Via::Spec, "S, the spec selects xyz1234b"),
            ("same pinout stands in", SAME, "XYZ1234", None, None, None, &pads, Some("a"), false, Via::Board, "U, nothing names it"),
            ("pinouts differ, stays open", OTHER, "XYZ1234", None, None, None, none, None, false, Via::Board, "selection required for U3: the 2 XYZ1234"),
            ("select contradicts the mpn", SAME, "XYZ1234", Some("XYZ1234ADBV"), None, Some("xyz1234b"), &pads, None, false, Via::Spec, "fits none of the 2"),
            ("package contradicts the mpn", SAME, "XYZ1234", Some("XYZ1234BDBV"), sot, None, &pads, None, false, Via::Board, "fits none of the 2"),
        ];
        for (case, b_pins, value, mpn, fp, select, pads, bound, determined, via, text) in cases {
            let lib = load(&family(b_pins, "SOIC-8")).unwrap();
            let owned = |t: Option<&str>| t.map(String::from);
            let mut q = ComponentQuery::default();
            (q.value, q.mpn, q.footprint) = (owned(Some(value)), owned(mpn), owned(fp));
            q.pads.clone_from(pads);
            q.properties
                .extend(select.map(|id| (SELECT_PROPERTY.into(), id.into())));
            let res = lib.resolve(&q);
            let s = res.selection.expect(case);
            let got = res.model.map(|m| m.id.replace("xyz1234", ""));
            let got = (got.as_deref(), s.determined.is_some(), s.via);
            assert_eq!(got, (bound, determined, via), "{case}: {}", s.listing());
            assert!(res.tied.is_empty(), "{case}: siblings are not a tie");
            let said = format!("{} {}", s.listing(), s.open_reason("U3"));
            assert!(said.contains(text), "{case}: {text} in {said}");
        }
    }

    #[test]
    fn the_lint_rejects_malformed_variant_sets() {
        let good = family(SAME, "SOT-23");
        let lint_src = |src: &str| {
            let file = toml::from_str::<crate::schema::DbFile>(src);
            let found = file.map_or_else(|e| vec![e.to_string()], |f| lint(&f.models));
            found.join("\n")
        };
        assert_eq!(lint_src(&good), "");
        let lone = format!("[[models]]{}", good.split("[[models]]").nth(1).unwrap());
        #[rustfmt::skip]
        let cases = [
            ("no [models.variant]", good.replacen("[models.variant]", "[models.variantx]", 1), "come together"),
            ("lone member", lone, "two or more entries"),
            ("shared code", good.replacen("^XYZ1234b", "^XYZ1234", 1), "no sibling's (also matches: xyz1234b)"),
            ("outside the family", good.replacen("XYZ1234aDBVR", "ABC1234aDBVR", 1), "orderable code ABC1234aDBVR"),
            ("no codes", good.replacen("[\"XYZ1234aDBV\", \"XYZ1234aDBVR\"]", "[]", 1), "ordering-table codes"),
            ("different match", good.replacen("(?i)^XYZ1234", "(?i)^XYZ12", 1), "one family match"),
            ("a default", good.replacen("basis = ", "default = true\nbasis = ", 1), "unknown field `default`"),
        ];
        for (case, src, want) in cases {
            assert!(lint_src(&src).contains(want), "{case}: {}", lint_src(&src));
            assert!(load(&src).is_err(), "{case} loads");
        }
    }
}
