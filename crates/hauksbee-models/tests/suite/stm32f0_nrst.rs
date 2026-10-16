//! The STM32F0 NRST pin has a permanent on-die pull-up (STM32F072x8/xB
//! `DocID025004` Rev 5, 6.3.15 p.83: "It is connected to a permanent pull-up
//! resistor, RPU."; Table 56 p.84: RPU 25 / 40 / 55 kOhm, VIN = VSS), so the
//! card names the pin `reset` and leaves it out of `must_not_float_roles`: a
//! removed external pull-up is not a fault on this part, while a reset pin with
//! no internal pull (the ADS122C04's RESET) is declared and judged.

use super::*;

#[test]
fn the_nrst_pin_is_a_reset_role_that_the_card_does_not_demand_a_pull_up_for() {
    let m = resolve_part("STM32F072CBT6").expect("STM32F072CBT6 resolves");
    assert_eq!(m.id, "stm32f072c");
    assert_pins(&m, &pins("7=reset"));
    assert!(
        m.params.get_str("must_not_float_roles").is_none(),
        "NRST has a permanent pull-up (Table 56 RPU 25..55 kOhm): it must not opt in to the float check"
    );
}

#[test]
fn a_reset_with_no_internal_pull_does_opt_in() {
    // Control: SBAS751B gives the ADS122C04 RESET no pull, so its card declares
    // the role and the float check applies.
    let m = expect_query(Some("ADC_ADS122C04RTE"), None, None);
    assert_eq!(m.params.get_str("must_not_float_roles"), Some("reset_n"));
}
