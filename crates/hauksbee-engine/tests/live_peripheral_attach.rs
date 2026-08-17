//! The browser's click-a-trace interaction must stamp a real peripheral into
//! the running circuit and advertise it back on BoardInfo. This is deliberately
//! a product-wire test rather than a React-only button test.

use hauksbee_engine::HauksbeeEngine;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_frontdoor_api::protocol::{LivePeripheralSpec, LiveRegisterMapSpec};
use std::collections::HashMap;

#[test]
fn clicked_trace_stimulus_is_real_controllable_and_fail_closed() {
    let board = include_str!("../../hauksbee-ci/examples/boards/tolerance_divider.kicad_pcb");
    let mut engine = HauksbeeEngine::from_board_file(board, None, "/boards/divider.kicad_pcb")
        .expect("build real board engine");
    let net = engine
        .board_info()
        .nets
        .into_iter()
        .find(|name| name != "GND" && name != "0")
        .expect("board has a driven net");

    engine
        .attach_peripheral(LivePeripheralSpec {
            id: "STIM_CLICK_1".into(),
            kind: "stimulus".into(),
            net: net.clone(),
            to: None,
            offset: Some(0.25),
            bounce_ms: None,
            initial: None,
        })
        .expect("attach a real 50-ohm stimulus");
    assert!(engine
        .board_info()
        .peripherals
        .iter()
        .any(|p| p.id == "STIM_CLICK_1" && p.kind == "stimulus"));
    assert!(engine.set_peripheral("STIM_CLICK_1", 0.75));
    let frame = engine.step(1e-4);
    assert!(frame.net_voltages.get(&net).is_some_and(|v| v.is_finite()));

    let duplicate = engine.attach_peripheral(LivePeripheralSpec {
        id: "STIM_CLICK_1".into(),
        kind: "stimulus".into(),
        net,
        to: None,
        offset: Some(0.0),
        bounce_ms: None,
        initial: None,
    });
    assert!(duplicate.unwrap_err().contains("already exists"));

    let missing_net = engine.attach_peripheral(LivePeripheralSpec {
        id: "BTN_BAD".into(),
        kind: "pushbutton".into(),
        net: "__not_a_board_net__".into(),
        to: Some("GND".into()),
        offset: None,
        bounce_ms: Some(5.0),
        initial: Some(0.0),
    });
    assert!(missing_net.unwrap_err().contains("does not exist"));
}

#[test]
fn exact_register_map_can_be_attached_live_and_bad_inputs_refuse() {
    let board = include_str!("../../hauksbee-ci/examples/boards/tolerance_divider.kicad_pcb");
    let mut engine = HauksbeeEngine::from_board_file(board, None, "/boards/divider.kicad_pcb")
        .expect("build real board engine");
    let spec_toml = r#"[sensor]
name = "Interactive temperature"
bus = "i2c"
i2c_address = 0x48
[[sensor.input]]
name = "temperature_c"
default = 25.0
[[sensor.register]]
addr = 0x00
bytes = 2
encoding = "q7.1_be"
expr = "temperature_c"
[sensor.protocol]
style = "i2c_pointer"
"#;
    engine
        .attach_register_map(LiveRegisterMapSpec {
            id: "U_TEMP".into(),
            request_id: None,
            spec_toml: spec_toml.into(),
            inputs: HashMap::from([("temperature_c".into(), 31.5)]),
            controller: None,
            cs_net: None,
        })
        .expect("validated register-map device attaches to live engine");
    assert!(engine
        .board_info()
        .peripherals
        .iter()
        .any(|peripheral| peripheral.id == "U_TEMP" && peripheral.kind == "i2c_bus"));

    let unknown = engine.attach_register_map(LiveRegisterMapSpec {
        id: "U_BAD_INPUT".into(),
        request_id: None,
        spec_toml: spec_toml.into(),
        inputs: HashMap::from([("tempereture_c".into(), 31.5)]),
        controller: None,
        cs_net: None,
    });
    assert!(unknown.unwrap_err().contains("not declared"));
}
