//! Orderable variants: sibling `[[models]]` entries that share `variant_of` and
//! the family match, told apart by `[models.variant]`.
//!
//! [`select`] scores each sibling against the component at the resolve tie
//! point, so a part name that pins no variant is never silently simulated as
//! whichever sibling sorts first.

use hauksbee_ir::evidence::{Assumption, ModelSource, ModelUncertainty};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::schema::{Characteristic, ModelEntry, ParamValue};
use crate::ComponentQuery;

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
///
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
    /// The stand-in's rows [`envelope`] widened, `param min .. typ .. max` each.
    pub widened: Vec<String>,
}

impl Selection {
    pub fn survivors(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| m.verdict != Verdict::C)
    }

    /// The member to bind: the determined one, else for a same-pinout set the
    /// first survivor as a flagged stand-in, else none (the part stays open).
    #[must_use]
    pub fn bound(&self) -> Option<&str> {
        let stand_in = self.survivors().find(|_| self.same_pinout).map(|m| &*m.id);
        self.determined.as_deref().or(stand_in)
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
    crate::package::package_body(package)?.family
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
        widened: Vec::new(),
    }
}

/// Widen the stand-in's printed rows to the family envelope of the members `of`.
///
/// Each row `model` shares with every member (a gain group's hFE, say) takes, in
/// its base unit, the lowest min and highest max for worst-case use and the
/// median typ as the nominal a plain run reads; `src` takes the widened
/// intervals. Returns each widened row as `param min .. typ .. max`.
pub fn envelope(model: &mut ModelEntry, src: &mut ModelSource, of: &[&ModelEntry]) -> Vec<String> {
    let mut widened: Vec<Characteristic> = Vec::new();
    for (key, value) in &mut model.params.0 {
        let base = |m: &&ModelEntry| match m.params.0.get(key) {
            Some(ParamValue::Row(row)) => crate::param_names::row_characteristic(key, row),
            _ => None,
        };
        let Some(rows) = of.iter().map(base).collect::<Option<Vec<_>>>() else {
            continue;
        };
        let mut row = rows[0].clone();
        row.basis = format!("family envelope of {} variants", rows.len());
        let sorted = |column: fn(&Characteristic) -> Option<f64>| {
            let mut v: Vec<f64> = rows.iter().map(column).collect::<Option<_>>()?;
            v.sort_by(f64::total_cmp);
            Some(v)
        };
        row.min = sorted(|r| r.min).map(|v| v[0]);
        row.typ = sorted(|r| r.typ).map(|v| v[(v.len() - 1) / 2]);
        row.max = sorted(|r| r.max).map(|v| v[v.len() - 1]);
        if row.min.or(row.typ).or(row.max).is_some() {
            *value = ParamValue::Row(Box::new(row.clone()));
            widened.push(row);
        }
    }
    let ours = |p: &String| widened.iter().any(|w| w.parameter == *p);
    let old = |u: &&ModelUncertainty| matches!(u, ModelUncertainty::Interval { parameter, .. } if ours(parameter));
    let kept = src.uncertainty().iter().filter(|u| !old(u)).cloned();
    let all = kept.chain(widened.iter().filter_map(Characteristic::interval));
    let (tier, layer, validation) = (src.tier(), src.layer(), src.validation());
    if let Ok(rebuilt) = ModelSource::new(tier, layer, src.origin(), validation, all.collect()) {
        *src = rebuilt;
    }
    let show = |v: Option<f64>| v.map_or_else(|| "?".to_string(), |v| v.to_string());
    let text = |w: &Characteristic| [w.min, w.typ, w.max].map(show).join(" .. ");
    let line = |w: &Characteristic| format!("{} {}", w.parameter, text(w));
    widened.iter().map(line).collect()
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
        let differ: Vec<&str> = siblings
            .iter()
            .filter(|e| e.r#match != *first)
            .map(|e| e.id.as_str())
            .collect();
        if siblings.len() < 2 {
            out.push(format!(
                "variant_of '{family}': {SET}; a single card drops variant_of and [models.variant]"
            ));
        } else if !differ.is_empty() {
            let (lead, ids) = (&siblings[0].id, differ.join(", "));
            out.push(format!("variant_of '{family}': {SET}, and {ids} differ from {lead}'s [models.match]: {SHARE}"));
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
                let why = [
                    (!hit(&patterns[i]), "its own orderable_re misses it"),
                    (
                        !hit(&family_re),
                        "the family's shared [models.match] misses it",
                    ),
                    (!others.is_empty(), "a sibling's orderable_re also takes it"),
                ];
                let why: Vec<&str> = why.iter().filter(|w| w.0).map(|w| w.1).collect();
                if !why.is_empty() {
                    let (id, also, why) = (&e.id, others.join(", "), why.join("; "));
                    out.push(format!(
                        "model '{id}': orderable code {code} {OWN} {also}): {why}"
                    ));
                }
            }
        }
    }
    out
}

const TOGETHER: &str = "variant_of and [models.variant] come together";
const SET: &str = "a variant set is two or more entries sharing one family match";
const SHARE: &str = "give every member the identical [models.match] (one value_re taking every member's orderable codes) and tell members apart only by [models.variant] orderable_re, orderable and package";
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

    /// Gain groups as variants: a board naming its group binds that group's hFE
    /// row; one naming none binds the family envelope and records the hedge; a
    /// single-group card resolves as before.
    #[test]
    fn a_gain_group_binds_its_hfe_row_or_the_hedged_family_envelope() {
        let group = |(g, [min, typ, max]): (char, [u32; 3])| {
            format!(
                "[[models]]\nid = \"xyz847{g}\"\nkind = \"bjt_npn\"\nvariant_of = \"XYZ847\"\n\
                 [models.match]\nvalue_re = \"(?i)^XYZ847\"\n[models.params]\nis = 1e-14\nnf = 1.0\n\
                 bf = {{ parameter = \"hFE group {g}\", min = {min}, typ = {typ}, max = {max}, unit = \"\", conditions = \"IC = 2 mA\", basis = \"p. 2\" }}\n\
                 [models.pins]\n\"1\" = \"base\"\n\"2\" = \"emitter\"\n\"3\" = \"collector\"\n\
                 [models.source]\ntier = \"datasheet-derived\"\n\
                 [models.variant]\norderable_re = \"{g}$\"\norderable = [\"XYZ847{g}\"]\nbasis = \"p. 1\"\n"
            )
        };
        #[rustfmt::skip]
        let family = [('a', [110, 180, 220]), ('b', [200, 290, 450]), ('c', [420, 520, 800])].map(group).concat();
        let single = group(('b', [200, 290, 450])).replace("variant_of = \"XYZ847\"\n", "");
        let single = &single[..single.find("[models.variant]").unwrap()];
        let hedged = ["family envelope", "gain-group suffix"];
        #[rustfmt::skip]
        let cases = [
            // (case, card, value) -> bound, bf read, bf interval and basis, hedge the assumption records
            ("the value names group B", &*family, "XYZ847B", "xyz847b", 290.0, (200.0, 450.0, "p. 2"), ""),
            ("no group named", &family, "XYZ847", "xyz847a", 290.0, (110.0, 800.0, "family envelope of 3 variants"), "bf 110 .. 290 .. 800"),
            ("a single-group card", single, "XYZ847", "xyz847b", 290.0, (200.0, 450.0, "p. 2"), ""),
        ];
        for (case, card, value, bound, bf, (low, high, basis), hedge) in cases {
            let q = ComponentQuery::new(None, value.to_string(), None);
            let res = load(card).unwrap().resolve(&q);
            let (model, source) = (res.model.expect(case), res.provenance.expect(case));
            let is_bf = |u: &&ModelUncertainty| matches!(u, ModelUncertainty::Interval { parameter, .. } if parameter == "bf");
            let got: Vec<_> = source.uncertainty().iter().filter(is_bf).collect();
            let want = ModelUncertainty::interval("bf", low, high, "-", basis).unwrap();
            let got = (&*model.id, model.params.get_f64("bf"), got);
            assert_eq!(got, (bound, Some(bf), vec![&want]), "{case}");
            let s = res.selection.filter(|s| s.determined.is_none());
            let (ids, rows) = s.map_or_else(Default::default, |s| (s.ids(), s.widened.join(", ")));
            let a = Assumption::undetermined_variant("Q1", "Q1", bound, &ids, &rows);
            let said = format!("{} {}", a.statement(), a.replacement());
            let told = hedge.is_empty() || hedged.iter().all(|w| said.contains(w));
            assert!(rows == hedge && told, "{case}: {said}");
        }
    }

    /// A stand-in may be the member whose pads a package standard numbers, so its pads stay assumed.
    #[test]
    fn a_stand_in_keeps_a_survivors_pin_standard() {
        let source = "[models.source]\ntier = \"datasheet-derived\"\n[[models.source.pinout]]\npackage = \"SOIC-8\"\nstandard = \"JEDEC top view\"\npins = { \"1\" = \"IN\", \"2\" = \"GND\", \"3\" = \"OUT\" }\n";
        let lib = load(&(family(SAME, "SOIC-8") + source)).unwrap();
        let q = ComponentQuery::new(None, "XYZ1234".to_string(), None);
        let model = lib.resolve(&q).model.unwrap();
        let got = Some((&*model.id, crate::validation::pin_standard(&model)));
        assert_eq!(got, Some(("xyz1234a", Some("JEDEC top view"))));
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
            ("lone member", lone, "a single card drops variant_of"),
            ("shared code", good.replacen("^XYZ1234b", "^XYZ1234", 1), "no sibling's (also matches: xyz1234b)"),
            ("outside the family", good.replacen("XYZ1234aDBVR", "ABC1234aDBVR", 1), "ABC1234aDBVR must match its own orderable_re and the family match and no sibling's (also matches: ): its own orderable_re misses it; the family's shared [models.match] misses it"),
            ("no codes", good.replacen("[\"XYZ1234aDBV\", \"XYZ1234aDBVR\"]", "[]", 1), "ordering-table codes"),
            ("different match", good.replacen("(?i)^XYZ1234", "(?i)^XYZ12", 1), "xyz1234b differ from xyz1234a's [models.match]: give every member the identical"),
            ("a default", good.replacen("basis = ", "default = true\nbasis = ", 1), "unknown field `default`"),
        ];
        for (case, src, want) in cases {
            assert!(lint_src(&src).contains(want), "{case}: {}", lint_src(&src));
            assert!(load(&src).is_err(), "{case} loads");
        }
    }
}
