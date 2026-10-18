//! The web app's buttons, derived from the typed data the engine already has.
//!
//! The CLI says what to do as a command (`hauksbee models extract
//! --unmodelled <board>`, `--bom bom.csv`, a `[[supply]]` block). A person in
//! the app needs the same step as a button. Nothing here invents advice: each
//! action comes from a typed source, the open-part list the verdict already
//! uses or an assumption's [`Remedy`], and maps onto the closed
//! [`hauksbee_frontdoor_api::actions::ActionId`] set the app has one handler
//! for. The prose the CLI prints is untouched.

use hauksbee_frontdoor_api::actions::{push_unique, WebAction};
use hauksbee_ir::evidence::{Assumption, Remedy};
use hauksbee_ir::vocabulary::NOT_JUDGED_JOINER;

/// The ways to give the open key parts models: one button for the parts, one
/// to draft all their models from datasheets.
#[must_use]
pub fn model_actions(refs: &[String]) -> Vec<WebAction> {
    if refs.is_empty() {
        return Vec::new();
    }
    vec![
        WebAction::model_parts(refs),
        WebAction::extract_unmodelled(refs),
    ]
}

/// The action one typed [`Remedy`] stands for.
///
/// Remedies that act on several parts at once are grouped by [`from_remedies`]
/// instead, so ten unfitted parts are one button, not ten.
#[must_use]
pub fn from_remedy(remedy: &Remedy) -> Option<WebAction> {
    match remedy {
        Remedy::AddSupply { net, volts_hint } => {
            Some(WebAction::add_supply(Some(net), *volts_hint))
        }
        Remedy::AddModel { reference } => {
            Some(WebAction::model_parts(std::slice::from_ref(reference)))
        }
        Remedy::SetPartNumber { reference } => {
            Some(WebAction::set_part_number(std::slice::from_ref(reference)))
        }
        Remedy::MarkDnp { reference } => {
            Some(WebAction::mark_not_fitted(std::slice::from_ref(reference)))
        }
        Remedy::AddFirmware => Some(WebAction::add_firmware()),
        Remedy::AddBom => Some(WebAction::add_bom()),
        Remedy::InstallBackend { id } => Some(WebAction::install_backend(
            id,
            &hauksbee_ir::labels::label_for(id),
        )),
        Remedy::ProvideProjectRules { file } => Some(WebAction::add_project_rules(file)),
        // Renaming a duplicate designator happens in the design tool; there is
        // no in-app step, so the assumption's own sentence stays the guidance.
        Remedy::RenameDuplicate { .. } | Remedy::Other => None,
    }
}

/// Every action the assumptions' remedies call for, deduplicated, with the
/// per-part remedies of one kind folded into one action that lists the parts.
#[must_use]
pub fn from_remedies<'a>(assumptions: impl IntoIterator<Item = &'a Assumption>) -> Vec<WebAction> {
    let mut models: Vec<String> = Vec::new();
    let mut part_numbers: Vec<String> = Vec::new();
    let mut unfitted: Vec<String> = Vec::new();
    let mut singles: Vec<WebAction> = Vec::new();
    let push = |list: &mut Vec<String>, reference: &String| {
        if !list.contains(reference) {
            list.push(reference.clone());
        }
    };
    for assumption in assumptions {
        match assumption.remedy() {
            Remedy::AddModel { reference } => push(&mut models, reference),
            Remedy::SetPartNumber { reference } => push(&mut part_numbers, reference),
            Remedy::MarkDnp { reference } => push(&mut unfitted, reference),
            other => {
                if let Some(action) = from_remedy(other) {
                    push_unique(&mut singles, action);
                }
            }
        }
    }
    let mut out = Vec::new();
    if !models.is_empty() {
        out.push(WebAction::model_parts(&models));
    }
    if !part_numbers.is_empty() {
        out.push(WebAction::set_part_number(&part_numbers));
    }
    if !unfitted.is_empty() {
        out.push(WebAction::mark_not_fitted(&unfitted));
    }
    for action in singles {
        push_unique(&mut out, action);
    }
    out
}

/// A verdict reason without its `; next: ...` clause.
///
/// [`crate::result::VerdictCause::reason`] joins what is wrong to what to do
/// about it with `; next: `, and the second half names terminal commands. The
/// web shows the first half as prose and the second as buttons, so this keeps
/// the first half and the `Also:` sentence that follows it (what was not
/// judged). A reason that does not have the shape (a refusal's own sentence,
/// the PARTLY JUDGED form) keeps its sentences, with any next step that names
/// a terminal command said in the web's words ([`hauksbee_ir::vocabulary::without_terminal_commands`]).
#[must_use]
pub fn reason_without_next(reason: &str) -> String {
    let without = match reason.split_once("; next: ") {
        None => reason.to_string(),
        Some((why, rest)) => match rest.split_once(NOT_JUDGED_JOINER) {
            Some((_, tail)) => format!("{why}.{NOT_JUDGED_JOINER}{tail}"),
            None => format!("{why}."),
        },
    };
    // What is left may still end on a next step worded as a command (the
    // PARTLY JUDGED form, and the sentence naming what a failing run did not
    // judge): the page says it in its own words.
    hauksbee_ir::vocabulary::without_terminal_commands(&without)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn open_parts_get_a_model_button_and_a_draft_button() {
        let actions = model_actions(&refs(&["U1", "Q2"]));
        let ids: Vec<_> = actions
            .iter()
            .map(|a| serde_json::to_value(a.id).unwrap())
            .collect();
        assert_eq!(ids, ["model_parts", "extract_unmodelled"]);
        assert!(model_actions(&[]).is_empty());
    }

    #[test]
    fn typed_remedies_map_onto_the_closed_action_set() {
        let supply = from_remedy(&Remedy::AddSupply {
            net: "+5V".into(),
            volts_hint: Some(4.5),
        })
        .unwrap();
        assert_eq!(supply.args.net.as_deref(), Some("+5V"));
        assert_eq!(supply.args.volts, Some(4.5));
        assert!(from_remedy(&Remedy::Other).is_none());
        assert!(from_remedy(&Remedy::RenameDuplicate {
            reference: "R1".into(),
            conflict: "x".into()
        })
        .is_none());
        let rules = from_remedy(&Remedy::ProvideProjectRules {
            file: ".kicad_pro".into(),
        })
        .unwrap();
        assert_eq!(rules.label, "Add the .kicad_pro file");
        let backend = from_remedy(&Remedy::InstallBackend {
            id: "ngspice".into(),
        })
        .unwrap();
        assert_eq!(backend.args.backend.as_deref(), Some("ngspice"));
    }

    #[test]
    fn a_reason_loses_its_terminal_next_step_and_keeps_what_was_not_judged() {
        assert_eq!(
            reason_without_next(
                "2 key parts have no usable model (U1, U2), so the checks cannot vouch for this board; next: draft their models with `hauksbee models extract --unmodelled <board>`, then rerun."
            ),
            "2 key parts have no usable model (U1, U2), so the checks cannot vouch for this board."
        );
        let with_tail =
            format!("1 blocking problem(s), first: a; next: fix a.{NOT_JUDGED_JOINER}U1 is open.");
        assert_eq!(
            reason_without_next(&with_tail),
            format!("1 blocking problem(s), first: a.{NOT_JUDGED_JOINER}U1 is open.")
        );
        assert_eq!(
            reason_without_next("PARTLY JUDGED: x."),
            "PARTLY JUDGED: x."
        );
    }

    #[test]
    fn a_partly_judged_reason_says_its_next_step_without_a_terminal_command() {
        use hauksbee_ir::vocabulary::{
            DRAFT_MODELS_NEXT_STEP, DRAFT_MODELS_NEXT_STEP_WEB, PARTLY_JUDGED_LEAD,
        };
        let cli = format!(
            "{PARTLY_JUDGED_LEAD}: nothing found blocks ordering among the checks this run \
             could judge. Q1 has no usable model, so the checks it could change are not \
             judged. {DRAFT_MODELS_NEXT_STEP}"
        );
        assert!(cli.contains('`'), "the CLI sentence keeps its command");
        let web = reason_without_next(&cli);
        assert!(!web.contains('`') && !web.contains("hauksbee models"), "{web}");
        assert!(web.starts_with(PARTLY_JUDGED_LEAD), "{web}");
        assert!(web.ends_with(DRAFT_MODELS_NEXT_STEP_WEB), "{web}");
        assert_eq!(
            DRAFT_MODELS_NEXT_STEP_WEB,
            "Add a model for each open part to judge the rest."
        );
        // The same step inside a failing run's "Also:" sentence.
        let failing = format!(
            "1 blocking problem(s), first: a; next: fix a.{NOT_JUDGED_JOINER}Q1 has no usable \
             model. {DRAFT_MODELS_NEXT_STEP}"
        );
        let web = reason_without_next(&failing);
        assert!(!web.contains('`'), "{web}");
        assert!(web.ends_with(DRAFT_MODELS_NEXT_STEP_WEB), "{web}");
    }
}
