//! Wire-contract regression tests: pin the exact JSON field names and shapes
//! the React frontend (frontend/src/types/protocol.ts) reads and writes, so a
//! rename on either side fails here instead of silently rendering `undefined`
//! in the UI. Two of these are fossils of real bugs: the frontend once read
//! `fault_kind` (the wire says `kind`) and treated `power_supplies` as an
//! array (the wire is a net -> config map).

use hauksbee_server::protocol::{
    BoardInfo, ClientMessage, FaultInfo, PowerSupplyConfig, ServerMessage, SimFrame, SupplyState,
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
