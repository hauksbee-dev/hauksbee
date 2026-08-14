//! Per-film X2 layer roles must be read before an opaque filename is dropped.

use std::path::{Path, PathBuf};

use hauksbee_extract::gerber::from_gerber_dir;

fn film(layer: usize, side: &str, x_mm: usize) -> String {
    format!(
        "%TF.FileFunction,Copper,L{layer},{side}*%\n\
         %FSLAX46Y46*%\n\
         %MOMM*%\n\
         %ADD10C,1.000000*%\n\
         D10*\n\
         X{}Y0D03*\n\
         M02*\n",
        x_mm * 1_000_000
    )
}

fn film_without_x2(x_mm: usize) -> String {
    format!(
        "%FSLAX46Y46*%\n\
         %MOMM*%\n\
         %ADD10C,1.000000*%\n\
         D10*\n\
         X{}Y0D03*\n\
         M02*\n",
        x_mm * 1_000_000
    )
}
fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hauksbee_gerber_x2_{tag}_{}", std::process::id()))
}

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write gerber fixture");
}

#[test]
fn x2_roles_recover_six_user_named_copper_films_without_a_job_manifest() {
    let dir = tmp("six_layer");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    write(&dir.join("board-F_Cu.gbr"), &film(1, "Top", 1));
    // None of these labels carries a stack index. Their own X2 declarations
    // are the only evidence that they are L2-L5 copper films.
    write(&dir.join("board-GND_Cu.gbr"), &film(2, "Inr", 2));
    write(&dir.join("board-Power_Cu.gbr"), &film(3, "Inr", 3));
    write(&dir.join("board-Signal_A_Cu.gbr"), &film(4, "Inr", 4));
    write(&dir.join("board-Signal_B_Cu.gbr"), &film(5, "Inr", 5));
    write(&dir.join("board-B_Cu.gbr"), &film(6, "Bot", 6));

    // `_Cu` alone is deliberately not a copper declaration. This file has no
    // X2 role and must remain out of the electrical stack.
    write(
        &dir.join("board-Notes_Cu.gbr"),
        "%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,1.000000*%\nD10*\nX99000000Y0D03*\nM02*\n",
    );
    // An X2-looking sentence in a non-film is not enough either. The fallback
    // requires structural RS-274X commands before it will trust the declaration.
    write(
        &dir.join("board-metadata.json"),
        "%TF.FileFunction,Copper,L7,Inr*%\n",
    );
    write(
        &dir.join("board-MissingSide_Cu.gbr"),
        "%TF.FileFunction,Copper,L7*%\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n",
    );
    write(
        &dir.join("board-JunkLayer_Cu.gbr"),
        "%TF.FileFunction,Copper,L8junk,NotSide*%\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n",
    );
    // Only a terminated TF file attribute has authority. A TA aperture
    // attribute or an unterminated lookalike must not promote an opaque film.
    write(
        &dir.join("board-ApertureAttribute_Cu.gbr"),
        "%TA.FileFunction,Copper,L9,Inr*%\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n",
    );
    write(
        &dir.join("board-Unterminated_Cu.gbr"),
        "%TF.FileFunction,Copper,L10,Inr\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n",
    );
    write(
        &dir.join("board-TrailingJunk_Cu.gbr"),
        "%TF.FileFunction,Copper,L11,Inr*%junk\n%FSLAX46Y46*%\n%MOMM*%\nM02*\n",
    );

    let extracted = from_gerber_dir(&dir).expect("extract user-labelled six-layer job");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(extracted.stats.n_layers, 6);
    assert_eq!(
        extracted.stats.total_flashes, 6,
        "the metadata-free Notes_Cu artwork must not be promoted by its name"
    );
    assert!(
        !extracted
            .stats
            .notes
            .iter()
            .any(|note| note.contains("only 2 copper layer")),
        "all six X2-declared films were recovered: {:?}",
        extracted.stats.notes
    );
}

#[test]
fn duplicate_x2_physical_layers_refuse_instead_of_choosing_by_file_order() {
    let dir = tmp("duplicate_layer");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    write(&dir.join("board-F_Cu.gbr"), &film(1, "Top", 1));
    write(&dir.join("board-GND_Cu.gbr"), &film(2, "Inr", 2));
    write(&dir.join("board-Power_Cu.gbr"), &film(2, "Inr", 3));
    write(&dir.join("board-B_Cu.gbr"), &film(4, "Bot", 4));

    let error = match from_gerber_dir(&dir) {
        Ok(_) => panic!("duplicate physical layer must refuse"),
        Err(error) => error,
    };
    let _ = std::fs::remove_dir_all(&dir);
    let message = error.to_string();
    assert!(message.contains("physical layer L2"), "{message}");
    assert!(
        message.contains("GND_Cu") && message.contains("Power_Cu"),
        "{message}"
    );
    assert!(message.contains("layer_map.txt"), "{message}");
}

#[test]
fn duplicate_x2_and_manifest_physical_layers_refuse_together() {
    let dir = tmp("duplicate_authorities");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    write(&dir.join("top.gbr"), &film_without_x2(1));
    write(&dir.join("manifest-inner.gbr"), &film_without_x2(2));
    write(&dir.join("x2-inner.gbr"), &film(2, "Inr", 3));
    write(&dir.join("bottom.gbr"), &film_without_x2(4));
    write(
        &dir.join("job.gbrjob"),
        r#"{"FilesAttributes": [
  {"Path": "top.gbr", "FileFunction": "Copper,L1,Top"},
  {"Path": "manifest-inner.gbr", "FileFunction": "Copper,L2,Inr"},
  {"Path": "x2-inner.gbr", "FileFunction": "Copper,L3,Inr"},
  {"Path": "bottom.gbr", "FileFunction": "Copper,L4,Bot"}
]}"#,
    );

    let error = match from_gerber_dir(&dir) {
        Ok(_) => panic!("X2 and manifest must not claim the same physical layer"),
        Err(error) => error,
    };
    let _ = std::fs::remove_dir_all(&dir);
    let message = error.to_string();
    assert!(message.contains("physical layer L2"), "{message}");
    assert!(
        message.contains("manifest-inner.gbr") && message.contains("x2-inner.gbr"),
        "{message}"
    );
    assert!(message.contains("layer_map.txt"), "{message}");
}
