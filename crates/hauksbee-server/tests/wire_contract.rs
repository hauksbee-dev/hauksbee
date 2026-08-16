//! Wire-contract regression tests: pin the exact JSON field names and shapes
//! the React frontend (frontend/src/types/protocol.ts) reads and writes, so a
//! rename on either side fails here instead of silently rendering `undefined`
//! in the UI. Two of these are fossils of real bugs: the frontend once read
//! `fault_kind` (the wire says `kind`) and treated `power_supplies` as an
//! array (the wire is a net -> config map).

use hauksbee_server::protocol::{
    BoardInfo, ClientMessage, FaultInfo, InputSourceInfo, PowerSupplyConfig, ServerMessage,
    SimFrame, SupplyState,
};

#[test]
fn fault_info_serializes_kind_not_fault_kind() {
    let mut frame = SimFrame::default();
    frame.faults.push(FaultInfo {
        component: "D1".into(),
        kind: "overcurrent".into(),
        value: 1.25,
        limit: 1.0,
        t: 0.5,
        destroyed: false,
    });
    let json = serde_json::to_value(ServerMessage::SimFrame(frame)).unwrap();
    let fault = &json["faults"][0];
    assert_eq!(fault["component"], "D1");
    // The frontend FaultPanel reads `kind`; `fault_kind` was a mock-server
    // invention that leaked into the TS mirror and rendered as undefined.
    assert_eq!(fault["kind"], "overcurrent");
    assert!(fault.get("fault_kind").is_none());
    assert_eq!(fault["value"], 1.25);
    assert_eq!(fault["limit"], 1.0);
    assert_eq!(fault["destroyed"], false);
}

#[test]
fn explicit_input_sources_and_live_attachments_match_the_browser_wire() {
    let mut info = BoardInfo {
        name: "b".into(),
        board_url: "/boards/b.kicad_pcb".into(),
        num_components: 0,
        num_nets: 1,
        nets: vec!["A0".into()],
        component_kinds: Default::default(),
        mcus: vec![],
        power_supplies: Default::default(),
        peripherals: vec![],
        input_sources: vec![InputSourceInfo {
            id: "A0".into(),
            kind: "voltage".into(),
            min: 0.0,
            max: 5.0,
            initial: 2.5,
            unit: "V".into(),
        }],
        shorts: None,
    };
    let json = serde_json::to_value(ServerMessage::BoardInfo(info.clone())).unwrap();
    assert_eq!(json["input_sources"][0]["id"], "A0");
    assert_eq!(json["input_sources"][0]["max"], 5.0);

    // A similarly named NET is not enough: only the explicit source list is
    // serialized. This guards the old frontend A0/INPUT-name guess.
    info.input_sources.clear();
    let no_sources = serde_json::to_value(ServerMessage::BoardInfo(info)).unwrap();
    assert!(no_sources.get("input_sources").is_none());

    let message: ClientMessage = serde_json::from_str(
        r#"{"type":"AttachPeripheral","id":"STIM_A0_1","kind":"stimulus","net":"A0","offset":0.25}"#,
    )
    .expect("browser live attachment parses");
    match message {
        ClientMessage::AttachPeripheral(spec) => {
            assert_eq!(spec.id, "STIM_A0_1");
            assert_eq!(spec.net, "A0");
            assert_eq!(spec.offset, Some(0.25));
        }
        other => panic!("expected AttachPeripheral, got {other:?}"),
    }

    let message: ClientMessage = serde_json::from_str(
        r#"{"type":"AttachRegisterMap","id":"U7_ACCEL","request_id":42,"spec_toml":"[sensor]\nname = \"WHOAMI\"\nbus = \"i2c\"\ni2c_address = 24\n[[sensor.register]]\naddr = 0\nconst = [19]\n[sensor.protocol]\nstyle = \"i2c_pointer\"\n","inputs":{"accel_x_g":0.25}}"#,
    )
    .expect("browser register-map attachment parses");
    match message {
        ClientMessage::AttachRegisterMap(spec) => {
            assert_eq!(spec.id, "U7_ACCEL");
            assert_eq!(spec.request_id, Some(42));
            assert!(spec.spec_toml.contains("i2c_pointer"));
            assert_eq!(spec.inputs["accel_x_g"], 0.25);
        }
        other => panic!("expected AttachRegisterMap, got {other:?}"),
    }

    let receipt = serde_json::to_value(ServerMessage::ActionResult {
        action: "attach_register_map".into(),
        id: "U7_ACCEL".into(),
        request_id: Some(42),
        ok: true,
        message: "Attached exact register-map bytes for U7_ACCEL to the live co-simulation.".into(),
    })
    .unwrap();
    assert_eq!(receipt["type"], "ActionResult");
    assert_eq!(receipt["request_id"], 42);
    assert_eq!(receipt["ok"], true);
}

#[test]
fn board_info_power_supplies_is_a_map_of_tagged_configs() {
    let mut info = BoardInfo {
        name: "b".into(),
        board_url: "/boards/b.kicad_pcb".into(),
        num_components: 0,
        num_nets: 0,
        nets: vec![],
        component_kinds: Default::default(),
        mcus: vec![],
        power_supplies: Default::default(),
        peripherals: vec![],
        input_sources: vec![],
        shorts: None,
    };
    info.power_supplies
        .insert("5V".into(), PowerSupplyConfig::Ideal { volts: 5.0 });
    let json = serde_json::to_value(ServerMessage::BoardInfo(info)).unwrap();
    // A MAP keyed by net name (not an array of names): the frontend iterates
    // Object.keys(). for..of over this object was an app-killing TypeError.
    assert!(json["power_supplies"].is_object());
    assert_eq!(json["power_supplies"]["5V"]["kind"], "ideal");
    assert_eq!(json["power_supplies"]["5V"]["volts"], 5.0);
}

#[test]
fn sim_frame_supply_states_shape_matches_the_frontend_reader() {
    let mut frame = SimFrame::default();
    frame.supply_states.insert(
        "VBAT".into(),
        SupplyState {
            kind: "battery".into(),
            current_a: 0.12,
            soc: 0.87,
        },
    );
    let json = serde_json::to_value(ServerMessage::SimFrame(frame)).unwrap();
    // PowerPanel reads frame.supply_states[net].soc (it once read a
    // `power_supply_soc` field that never existed on the wire).
    assert_eq!(json["supply_states"]["VBAT"]["soc"], 0.87);
    assert_eq!(json["supply_states"]["VBAT"]["current_a"], 0.12);
    assert_eq!(json["supply_states"]["VBAT"]["kind"], "battery");
    assert!(json.get("power_supply_soc").is_none());

    // Empty maps/lists are omitted entirely; the frontend must optional-chain.
    let empty = serde_json::to_value(ServerMessage::SimFrame(SimFrame::default())).unwrap();
    assert!(empty.get("supply_states").is_none());
    assert!(empty.get("faults").is_none());
}

#[test]
fn set_power_supply_accepts_exactly_what_the_power_panel_sends() {
    // These literals mirror frontend/src/lib/supply-wire.ts `toWireSupply`.
    let cases = [
        r#"{"type":"SetPowerSupply","net":"5V","supply":{"kind":"ideal","volts":5}}"#,
        r#"{"type":"SetPowerSupply","net":"5V","supply":{"kind":"bench","volts":5,"current_limit_a":1.5}}"#,
        r#"{"type":"SetPowerSupply","net":"5V","supply":{"kind":"wall","volts":5,"r_out_ohms":0.5,"ripple_vpp":0.1,"ripple_hz":100}}"#,
        r#"{"type":"SetPowerSupply","net":"5V","supply":{"kind":"usb","spec":"v5_1_5a"}}"#,
        r#"{"type":"SetPowerSupply","net":"VBAT","supply":{"kind":"battery","chemistry":"li_ion","cells":1,"capacity_mah":2000,"soc":1,"r_internal_ohms":0.1}}"#,
    ];
    for case in cases {
        let msg: ClientMessage = serde_json::from_str(case)
            .unwrap_or_else(|e| panic!("frontend payload rejected: {case}: {e}"));
        assert!(
            matches!(msg, ClientMessage::SetPowerSupply { .. }),
            "wrong variant for {case}"
        );
    }
}

#[test]
fn set_controls_without_destructive_faults_still_parses() {
    // The frontend's SolverControls omits destructive_faults unless the user
    // touched it; the server must default it, not reject the message.
    let json = r#"{"type":"SetControls","temperature_c":27.0,"parasitics":false,
                   "junction_caps":true,"tolerances":false,"integration":"trap",
                   "fixed_dt":0.0,"granularity":1.0}"#;
    let msg: ClientMessage = serde_json::from_str(json).unwrap();
    match msg {
        ClientMessage::SetControls(c) => assert!(!c.destructive_faults),
        other => panic!("expected SetControls, got {other:?}"),
    }
}
