//! TEMPORARY diagnostic (deleted before hand-off): dump the watchy board's
//! MCU pin-driver map to understand which nets carry drivers.

use hauksbee_engine::engine::HauksbeeEngine;

#[test]
fn dump_watchy_driver_map() {
    let board = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../demo/sessions/watchy/watchy.kicad_pcb");
    let text = std::fs::read_to_string(board).unwrap();
    let fw = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../demo/firmware/watchy_display_init_s3/flash.bin");
    let mut engine = HauksbeeEngine::from_board_file(&text, Some(&fw), "x").unwrap();
    {
        use hauksbee_server::engine::Engine;
        for _ in 0..5 {
            let _ = engine.step(0.01);
        }
    }
    let sched = engine.scheduler();
    for net in [
        "MOSI", "SCK", "SDA", "SCL", "RES", "DC", "CS", "STAT", "USB_DET",
    ] {
        println!(
            "net {net}: mcu_pin_for_net = {:?}, voltage_known = {:?}",
            sched.mcu_pin_for_net(net),
            sched.net_voltage(net).is_some()
        );
    }
    println!("unobserved = {:?}", sched.unobserved_drive_nets());
    println!("mcu identities = {:?}", sched.mcu_identities());
}
