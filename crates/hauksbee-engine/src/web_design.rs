//! Shared preparation for the complete local-web design bundle.
//!
//! Report, Checks, and Live Sim receive the same [`DesignUpload`]. This module
//! applies manufacturing identity and model context once for the in-process
//! report/live paths; the Checks path stages the same files for `hauksbee-ci`.

use std::io::Write as _;

use hauksbee_frontdoor_api::frontdoor::{DesignUpload, NamedUpload};
use hauksbee_ir::evidence::{
    ArtifactKind, ArtifactProvenance, ArtifactRole, Contribution, IgnoredInput,
};
use hauksbee_models::ModelLibrary;
use sha2::{Digest as _, Sha256};

use crate::binder::{bind_board, BoundBoard};
use crate::board_input::NormalizedBoard;
use crate::schematic_ties::SchematicTies;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserVariant {
    name: String,
    #[serde(default)]
    fit: Vec<String>,
    #[serde(default)]
    no_fit: Vec<String>,
}

pub struct PreparedWebDesign {
    pub upload: DesignUpload,
    pub norm: NormalizedBoard,
    pub ties: Option<SchematicTies>,
    pub lib: ModelLibrary,
    pub asbuilt: Option<crate::asbuilt::AsBuiltOverlay>,
    /// Exact supplemental inputs that shaped the prepared design. The report
    /// evidence spine consumes these so browser runs are as reproducible as CI.
    pub supporting_artifacts: Vec<ArtifactProvenance>,
    /// Model files must remain on disk while the library and live engine use
    /// them. `None` when no browser model files were supplied.
    pub keepalive: Option<tempfile::TempDir>,
}

impl PreparedWebDesign {
    pub fn bind(&self) -> Result<BoundBoard, String> {
        let mut bound = bind_board(&self.norm.board, &self.lib);
        if let Some(overlay) = &self.asbuilt {
            overlay
                .apply(&mut bound)
                .map_err(|error| error.to_string())?;
        }
        Ok(bound)
    }
}

fn safe_leaf(name: &str, fallback: &str) -> String {
    std::path::Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .unwrap_or(fallback)
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn stage_models(models: &[NamedUpload]) -> Result<Option<tempfile::TempDir>, String> {
    if models.is_empty() {
        return Ok(None);
    }
    let dir = tempfile::Builder::new()
        .prefix("hauksbee-web-models-")
        .tempdir()
        .map_err(|error| format!("could not stage browser model files: {error}"))?;
    let mut used = std::collections::BTreeSet::new();
    for (index, model) in models.iter().enumerate() {
        let leaf = safe_leaf(&model.name, &format!("model-{index}.toml"));
        if !leaf.to_ascii_lowercase().ends_with(".toml") {
            return Err(format!(
                "model file '{}' is not TOML; select model-library .toml files",
                model.name
            ));
        }
        if !used.insert(leaf.to_ascii_lowercase()) {
            return Err(format!(
                "duplicate model filename '{leaf}' in the browser upload"
            ));
        }
        let path = dir.path().join(&leaf);
        let mut file = std::fs::File::create(&path)
            .map_err(|error| format!("could not stage model '{leaf}': {error}"))?;
        file.write_all(&model.bytes)
            .and_then(|_| file.flush())
            .map_err(|error| format!("could not write model '{leaf}': {error}"))?;
    }
    Ok(Some(dir))
}

pub fn prepare(upload: DesignUpload) -> Result<PreparedWebDesign, String> {
    let mut norm = crate::board_input::from_bytes(&upload.board.name, &upload.board.bytes)
        .map_err(|error| error.web_message())?;
    let design_identity = norm.board.clone();
    let board_is_eagle = norm
        .layout_text
        .as_deref()
        .is_some_and(|text| text.contains("<eagle") && text.contains("<board"));
    let schematic = upload
        .schematic
        .as_ref()
        .map(|file| (file.name.as_str(), file.bytes.as_slice()));
    let ties = crate::schematic_ties::resolve_uploaded(
        &upload.board.name,
        &design_identity,
        schematic,
        board_is_eagle,
    )
    .map_err(|error| error.to_string())?;

    let keepalive = stage_models(&upload.models)?;
    let extra: Vec<&std::path::Path> = keepalive
        .as_ref()
        .map(|dir| vec![dir.path()])
        .unwrap_or_default();
    let lib = ModelLibrary::builtin_with_user_dirs(&extra);

    let mut supporting_artifacts = Vec::new();
    if let Some(file) = &upload.bom {
        let bom = hauksbee_extract::bom::Bom::from_bytes(
            &file.bytes,
            &file.name,
            &hauksbee_extract::bom::ColumnOverrides::new(),
        )
        .map_err(|error| error.to_string())?;
        let identity = crate::binder::apply_bom_identity(&mut norm.board, &bom, &lib)
            .map_err(|error| error.to_string())?;
        let mut contributions: Vec<Contribution> = bom
            .provenance
            .contributed
            .iter()
            .map(|item| Contribution {
                what: item.what.clone(),
                detail: item.detail.clone(),
            })
            .collect();
        contributions.extend(identity.lines().into_iter().map(|detail| Contribution {
            what: "identity_reconciliation".into(),
            detail,
        }));
        let artifact = ArtifactProvenance::new(
            &bom.provenance.path,
            ArtifactKind::Bom,
            ArtifactRole::Bom,
            &bom.provenance.sha256,
            Vec::new(),
        )
        .map_err(|error| error.to_string())?
        .with_format(bom.provenance.kind.clone())
        .with_contributions(contributions)
        .with_ignored(
            bom.provenance
                .ignored
                .iter()
                .map(|item| IgnoredInput {
                    what: item.what.clone(),
                    why: item.why.clone(),
                })
                .collect(),
        );
        supporting_artifacts.push(artifact);
    }
    if let Some(file) = &upload.placement {
        let placement =
            hauksbee_extract::placement::PlacementFile::from_bytes(&file.bytes, &file.name)
                .map_err(|error| error.to_string())?;
        let identity = crate::binder::apply_placement_identity(&mut norm.board, &placement, &lib)
            .map_err(|error| error.to_string())?;
        let mut contributions: Vec<Contribution> = placement
            .provenance
            .contributed
            .iter()
            .map(|item| Contribution {
                what: item.what.clone(),
                detail: item.detail.clone(),
            })
            .collect();
        contributions.extend(identity.lines().into_iter().map(|detail| Contribution {
            what: "identity_reconciliation".into(),
            detail,
        }));
        let artifact = ArtifactProvenance::new(
            &placement.provenance.path,
            ArtifactKind::Placement,
            ArtifactRole::Placement,
            &placement.provenance.sha256,
            Vec::new(),
        )
        .map_err(|error| error.to_string())?
        .with_format(placement.provenance.kind.clone())
        .with_contributions(contributions)
        .with_ignored(
            placement
                .provenance
                .ignored
                .iter()
                .map(|item| IgnoredInput {
                    what: item.what.clone(),
                    why: item.why.clone(),
                })
                .collect(),
        );
        supporting_artifacts.push(artifact);
    }
    let variant = upload
        .variant
        .as_ref()
        .map(|file| {
            let text = std::str::from_utf8(&file.bytes)
                .map_err(|_| format!("assembly variant '{}' is not UTF-8 TOML", file.name))?;
            let variant: BrowserVariant = toml::from_str(text)
                .map_err(|error| format!("assembly variant '{}': {error}", file.name))?;
            if variant.name.trim().is_empty() {
                return Err(format!(
                    "assembly variant '{}' has an empty name",
                    file.name
                ));
            }
            Ok(variant)
        })
        .transpose()?;
    if let (Some(file), Some(variant)) = (&upload.variant, &variant) {
        supporting_artifacts.push(
            ArtifactProvenance::new(
                &file.name,
                ArtifactKind::Toml,
                ArtifactRole::Variant,
                sha256_hex(&file.bytes),
                Vec::new(),
            )
            .map_err(|error| error.to_string())?
            .with_contributions(vec![Contribution {
                what: "assembly_variant".into(),
                detail: format!(
                    "variant {:?}: {} explicit fit and {} explicit no-fit references",
                    variant.name,
                    variant.fit.len(),
                    variant.no_fit.len()
                ),
            }]),
        );
    }
    norm.board
        .apply_dnp_policy(
            Default::default(),
            variant
                .as_ref()
                .map(|v| v.fit.as_slice())
                .unwrap_or_default(),
            variant
                .as_ref()
                .map(|v| v.no_fit.as_slice())
                .unwrap_or_default(),
        )
        .map_err(|error| error.to_string())?;
    let asbuilt = if let Some(file) = &upload.asbuilt {
        let text = std::str::from_utf8(&file.bytes)
            .map_err(|_| format!("as-built overlay '{}' is not UTF-8 TOML", file.name))?;
        let overlay = crate::asbuilt::AsBuiltOverlay::parse(text, &file.name)
            .map_err(|error| error.to_string())?;
        let mut validation_bound = bind_board(&norm.board, &lib);
        overlay
            .apply(&mut validation_bound)
            .map_err(|error| error.to_string())?;
        supporting_artifacts.push(
            ArtifactProvenance::new(
                &file.name,
                ArtifactKind::Toml,
                ArtifactRole::AsBuilt,
                sha256_hex(&file.bytes),
                Vec::new(),
            )
            .map_err(|error| error.to_string())?
            .with_contributions(vec![Contribution {
                what: "as_built_overlay".into(),
                detail: "unit-specific fitted parts, measured values, and wiring overrides applied after binding".into(),
            }]),
        );
        Some(overlay)
    } else {
        None
    };

    for model in &upload.models {
        supporting_artifacts.push(
            ArtifactProvenance::new(
                &model.name,
                ArtifactKind::Toml,
                ArtifactRole::ModelPack,
                sha256_hex(&model.bytes),
                Vec::new(),
            )
            .map_err(|error| error.to_string())?
            .with_contributions(vec![Contribution {
                what: "model_resolution".into(),
                detail: "selected browser model entry participated in the model-resolution ladder"
                    .into(),
            }]),
        );
    }

    Ok(PreparedWebDesign {
        upload,
        norm,
        ties,
        lib,
        asbuilt,
        supporting_artifacts,
        keepalive,
    })
}

#[cfg(test)]
mod tests {
    use super::{prepare, safe_leaf};
    use hauksbee_frontdoor_api::frontdoor::{DesignUpload, NamedUpload};
    use hauksbee_ir::evidence::ArtifactRole;

    #[test]
    fn uploaded_model_names_cannot_escape_the_staging_directory() {
        assert_eq!(
            safe_leaf("../outside.toml", "fallback.toml"),
            "outside.toml"
        );
        assert_eq!(
            safe_leaf("folder/model.toml", "fallback.toml"),
            "model.toml"
        );
    }

    #[test]
    fn complete_browser_bundle_applies_identity_models_and_overlay() {
        let upload = DesignUpload {
            board: NamedUpload {
                name: "button_pullup.kicad_pcb".into(),
                bytes: include_bytes!("../../../testdata/boards/button_pullup.kicad_pcb").to_vec(),
            },
            firmware: None,
            schematic: None,
            bom: Some(NamedUpload {
                name: "bom.csv".into(),
                bytes: b"Designator,Value\nR1,10k\n".to_vec(),
            }),
            placement: Some(NamedUpload {
                name: "placement.csv".into(),
                bytes: b"Designator,Mid X,Mid Y,Rotation,Layer\nR1,100,100,0,top\n".to_vec(),
            }),
            variant: Some(NamedUpload {
                name: "production.variant.toml".into(),
                bytes: b"name = \"production\"\n".to_vec(),
            }),
            asbuilt: Some(NamedUpload {
                name: "unit.asbuilt.toml".into(),
                bytes: Vec::new(),
            }),
            models: vec![NamedUpload {
                name: "passives.toml".into(),
                bytes: include_bytes!("../../hauksbee-models/db/passives.toml").to_vec(),
            }],
        };
        let prepared = prepare(upload).expect("the complete browser bundle prepares");
        assert!(prepared
            .keepalive
            .as_ref()
            .unwrap()
            .path()
            .join("passives.toml")
            .is_file());
        let roles: Vec<_> = prepared
            .supporting_artifacts
            .iter()
            .map(|artifact| artifact.role())
            .collect();
        assert_eq!(
            roles,
            vec![
                ArtifactRole::Bom,
                ArtifactRole::Placement,
                ArtifactRole::Variant,
                ArtifactRole::AsBuilt,
                ArtifactRole::ModelPack,
            ]
        );
        let bound = prepared
            .bind()
            .expect("the overlay applies to the same bound board");
        assert!(bound.report.rows.iter().any(|row| row.reference == "R1"));
    }
}
