//! Structured next steps for the web app.
//!
//! A finding, a report section or a verdict reason can say what to do about it
//! in two ways. The terminal says it in prose that names a command (`hauksbee
//! models extract --unmodelled <board>`, `hauksbee-ci init`, a TOML block to
//! paste); that text stays for the CLI and its JSON. A person already inside
//! the app has no use for a command: they need a button. [`WebAction`] is that
//! button's contract: an id from the small closed [`ActionId`] set, the words
//! to put on the button, and the arguments the app needs to start the flow
//! (which parts, which net, which file).
//!
//! The set is closed on purpose. The app owns one handler per id; an engine
//! that wants a step the app has no handler for adds an id here, so the two
//! sides cannot drift into free-text instructions again. The wire shape is
//! additive: an `actions` list is omitted when empty, and an older app simply
//! never reads it.

use serde::Serialize;

/// What an action does, as the app's handlers are keyed. Serialised in
/// `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionId {
    /// Open the model flow for the parts in `args.refs`.
    ModelParts,
    /// Draft models from the datasheets of every unmodelled key part
    /// (`args.refs` lists them).
    ExtractUnmodelled,
    /// Attach a bill of materials to this design.
    AddBom,
    /// Point the analysis at a folder of model files.
    ChooseModelsFolder,
    /// Add a load-step scenario to the checks.
    AddScenario,
    /// Declare a supply on `args.net`, at `args.volts` when known.
    AddSupply,
    /// Add a small-signal frequency sweep to the checks.
    AddAcSweep,
    /// Load a firmware image.
    AddFirmware,
    /// Open the check of kind `args.check`, prefilled for `args.net` or the
    /// part in `args.refs`.
    OpenCheck,
    /// Pan and highlight on the board: the parts in `args.refs`, the net
    /// `args.net`, or the point `args.x`, `args.y` (mm, layout space).
    ShowOnBoard,
    /// Open the checks view to compose a first set of checks.
    StartChecks,
    /// Upload the project rules file `args.file` (a `.kicad_pro`) with the board.
    AddProjectRules,
    /// Put the manufacturer part number on the parts in `args.refs`.
    SetPartNumber,
    /// Mark the parts in `args.refs` not fitted.
    MarkNotFitted,
    /// Install the simulator backend `args.backend`.
    InstallBackend,
}

/// The arguments an action's handler reads. Every field is omitted when unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ActionArgs {
    /// Reference designators the action is about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    /// A net name, as the board spells it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<String>,
    /// A check kind from the check catalogue (`voltage`, `rail_window`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// A file name the action asks for (`.kicad_pro`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// A simulator backend id (`ngspice`, `renode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// A supply voltage, V.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volts: Option<f64>,
    /// A board position, mm, layout space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
}

impl ActionArgs {
    /// Whether no argument is set (the `args` object is then left out).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One thing the app can offer as a button.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WebAction {
    pub id: ActionId,
    /// The words on the button, in plain engineering English.
    pub label: String,
    #[serde(default, skip_serializing_if = "ActionArgs::is_empty")]
    pub args: ActionArgs,
}

/// "U1", "U1 and U2", "U1, U2 and 3 more": at most two names, the rest counted.
fn parts_phrase(refs: &[String]) -> String {
    match refs {
        [] => "these parts".to_string(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

impl WebAction {
    fn new(id: ActionId, label: impl Into<String>, args: ActionArgs) -> Self {
        Self {
            id,
            label: label.into(),
            args,
        }
    }

    fn refs(refs: &[String]) -> ActionArgs {
        ActionArgs {
            refs: refs.to_vec(),
            ..ActionArgs::default()
        }
    }

    /// "Give U1 a model".
    #[must_use]
    pub fn model_parts(refs: &[String]) -> Self {
        let label = format!("Give {} a model", parts_phrase(refs));
        Self::new(ActionId::ModelParts, label, Self::refs(refs))
    }

    /// Draft models from the datasheets of the parts the verdict is missing.
    #[must_use]
    pub fn extract_unmodelled(refs: &[String]) -> Self {
        let label = match refs {
            [one] => format!("Draft a model for {one} from its datasheet"),
            _ => "Draft models from the datasheets".to_string(),
        };
        Self::new(ActionId::ExtractUnmodelled, label, Self::refs(refs))
    }

    #[must_use]
    pub fn add_bom() -> Self {
        Self::new(
            ActionId::AddBom,
            "Add a bill of materials",
            ActionArgs::default(),
        )
    }

    #[must_use]
    pub fn choose_models_folder() -> Self {
        Self::new(
            ActionId::ChooseModelsFolder,
            "Choose a models folder",
            ActionArgs::default(),
        )
    }

    #[must_use]
    pub fn add_scenario() -> Self {
        Self::new(
            ActionId::AddScenario,
            "Add a load-step test",
            ActionArgs::default(),
        )
    }

    /// "Add a supply on +5V", or "Add a supply" when no net is known yet.
    #[must_use]
    pub fn add_supply(net: Option<&str>, volts: Option<f64>) -> Self {
        Self::new(
            ActionId::AddSupply,
            net.map_or_else(
                || "Add a supply".to_string(),
                |n| format!("Add a supply on {n}"),
            ),
            ActionArgs {
                net: net.map(str::to_string),
                volts,
                ..ActionArgs::default()
            },
        )
    }

    #[must_use]
    pub fn add_ac_sweep() -> Self {
        Self::new(
            ActionId::AddAcSweep,
            "Add a frequency sweep",
            ActionArgs::default(),
        )
    }

    #[must_use]
    pub fn add_firmware() -> Self {
        Self::new(ActionId::AddFirmware, "Add firmware", ActionArgs::default())
    }

    /// Open a check from the catalogue, titled by the caller.
    #[must_use]
    pub fn open_check(label: &str, check: &str, net: Option<&str>, refs: &[String]) -> Self {
        Self::new(
            ActionId::OpenCheck,
            label,
            ActionArgs {
                refs: refs.to_vec(),
                net: net.map(str::to_string),
                check: Some(check.to_string()),
                ..ActionArgs::default()
            },
        )
    }

    /// Show a point on the board.
    #[must_use]
    pub fn show_at(x: f64, y: f64) -> Self {
        Self::new(
            ActionId::ShowOnBoard,
            "Show on board",
            ActionArgs {
                x: Some(x),
                y: Some(y),
                ..ActionArgs::default()
            },
        )
    }

    /// Show parts on the board.
    #[must_use]
    pub fn show_parts(refs: &[String]) -> Self {
        Self::new(ActionId::ShowOnBoard, "Show on board", Self::refs(refs))
    }

    /// Show a net on the board.
    #[must_use]
    pub fn show_net(net: &str) -> Self {
        Self::new(
            ActionId::ShowOnBoard,
            "Show on board",
            ActionArgs {
                net: Some(net.to_string()),
                ..ActionArgs::default()
            },
        )
    }

    #[must_use]
    pub fn start_checks() -> Self {
        Self::new(
            ActionId::StartChecks,
            "Set up checks",
            ActionArgs::default(),
        )
    }

    /// "Add the `.kicad_pro` file".
    #[must_use]
    pub fn add_project_rules(file: &str) -> Self {
        Self::new(
            ActionId::AddProjectRules,
            format!("Add the {file} file"),
            ActionArgs {
                file: Some(file.to_string()),
                ..ActionArgs::default()
            },
        )
    }

    #[must_use]
    pub fn set_part_number(refs: &[String]) -> Self {
        let label = format!("Set the part number of {}", parts_phrase(refs));
        Self::new(ActionId::SetPartNumber, label, Self::refs(refs))
    }

    #[must_use]
    pub fn mark_not_fitted(refs: &[String]) -> Self {
        let label = format!("Mark {} as not fitted", parts_phrase(refs));
        Self::new(ActionId::MarkNotFitted, label, Self::refs(refs))
    }

    /// Install a simulator backend; `name` is its plain name ("ngspice").
    #[must_use]
    pub fn install_backend(id: &str, name: &str) -> Self {
        Self::new(
            ActionId::InstallBackend,
            format!("Install {name}"),
            ActionArgs {
                backend: Some(id.to_string()),
                ..ActionArgs::default()
            },
        )
    }
}

/// Append `action` unless one with the same id and arguments is already there.
pub fn push_unique(list: &mut Vec<WebAction>, action: WebAction) {
    if !list
        .iter()
        .any(|a| a.id == action.id && a.args == action.args)
    {
        list.push(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn labels_name_a_few_parts_and_count_the_rest() {
        assert_eq!(
            WebAction::model_parts(&refs(&["U1"])).label,
            "Give U1 a model"
        );
        assert_eq!(
            WebAction::model_parts(&refs(&["U1", "U2"])).label,
            "Give U1 and U2 a model"
        );
        assert_eq!(
            WebAction::model_parts(&refs(&["U1", "U2", "Q1", "Q2"])).label,
            "Give U1, U2 and 2 more a model"
        );
    }

    #[test]
    fn wire_shape_is_snake_case_and_omits_unset_arguments() {
        let json = serde_json::to_value(WebAction::add_supply(Some("+5V"), Some(5.0))).unwrap();
        assert_eq!(json["id"], "add_supply");
        assert_eq!(json["args"]["net"], "+5V");
        assert!(json["args"].get("refs").is_none());
        let bare = serde_json::to_value(WebAction::add_bom()).unwrap();
        assert_eq!(bare["id"], "add_bom");
        assert!(bare.get("args").is_none(), "{bare}");
    }

    #[test]
    fn push_unique_drops_an_identical_action_only() {
        let mut list = Vec::new();
        push_unique(&mut list, WebAction::add_bom());
        push_unique(&mut list, WebAction::add_bom());
        push_unique(&mut list, WebAction::show_net("GND"));
        push_unique(&mut list, WebAction::show_net("VCC"));
        assert_eq!(list.len(), 3);
    }
}
