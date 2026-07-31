//! The BC307 PNP entry must not swallow the NPN BC317/318/319 parts (wrong
//! polarity), and the BC237 NPN entry must not over-match BC240-259.

use hauksbee_models::{ComponentQuery, ModelLibrary};

fn resolved_id(lib: &ModelLibrary, val: &str) -> Option<String> {
    let q = ComponentQuery {
        value: Some(val.into()),
        ..Default::default()
    };
    lib.resolve(&q).model.map(|m| m.id.clone())
}

#[test]
fn bc3xx_pnp_entry_excludes_npn_bc317_318_319() {
    let lib = ModelLibrary::builtin();
    // Genuine PNP parts still resolve to the PNP entry.
    for v in [
        "BC307", "BC308", "BC309", "BC320", "BC327", "BC328", "BC557",
    ] {
        assert_eq!(
            resolved_id(&lib, v).as_deref(),
            Some("bc307"),
            "{v} should be the PNP entry"
        );
    }
    // NPN parts must NOT be modelled as the PNP bc307 (wrong polarity).
    for v in ["BC317", "BC318", "BC319"] {
        assert_ne!(
            resolved_id(&lib, v).as_deref(),
            Some("bc307"),
            "{v} is NPN and must not resolve to the PNP bc307 entry"
        );
    }
}

#[test]
fn bc2xx_npn_entry_is_not_over_broad() {
    let lib = ModelLibrary::builtin();
    for v in ["BC237", "BC238", "BC239", "BC547", "BC548"] {
        assert_eq!(
            resolved_id(&lib, v).as_deref(),
            Some("bc237"),
            "{v} should be the NPN entry"
        );
    }
    // BC240-259 are a different family and must not bind here.
    for v in ["BC240", "BC250", "BC259"] {
        assert_ne!(
            resolved_id(&lib, v).as_deref(),
            Some("bc237"),
            "{v} should not match bc237"
        );
    }
}
