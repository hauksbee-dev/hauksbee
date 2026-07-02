//! Integration tests against hauksbee's own demo firmware
//! (testdata/firmware/demo): banner on boot, UART echo/commands, LED blink
//! on PB5, ADC readback — every co-sim coupling path with firmware we
//! control end to end.

use hauksbee_mcu::{AvrMcu, Mcu, PinId};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn demo_hex() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/firmware/demo/demo.hex");
    p.exists().then_some(p)
}

fn boot() -> Option<(AvrMcu, Arc<Mutex<Vec<u8>>>)> {
    let hex = demo_hex()?;
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&hex).expect("load demo.hex");
    let rx: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink = rx.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
    Some((mcu, rx))
}

fn uart_string(rx: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&rx.lock().unwrap()).into_owned()
}

#[test]
fn boot_banner() {
    let Some((mut mcu, rx)) = boot() else {
        eprintln!("SKIP: demo.hex missing");
        return;
    };
    mcu.run_millis(50).unwrap();
    assert!(
        uart_string(&rx).contains("hauksbee-demo v1"),
        "banner missing, got {:?}",
        uart_string(&rx)
    );
}

#[test]
fn uart_echo_and_ident() {
    let Some((mut mcu, rx)) = boot() else {
        eprintln!("SKIP: demo.hex missing");
        return;
    };
    mcu.run_millis(30).unwrap();
    rx.lock().unwrap().clear();

    mcu.uart_write(b"x");
    mcu.run_millis(30).unwrap();
    assert!(
        uart_string(&rx).contains('x'),
        "echo failed: {:?}",
        uart_string(&rx)
    );

    rx.lock().unwrap().clear();
    mcu.uart_write(b"i");
    mcu.run_millis(30).unwrap();
    assert!(
        uart_string(&rx).contains("hauksbee-demo v1"),
        "ident failed: {:?}",
        uart_string(&rx)
    );
}

#[test]
fn led_blinks_on_pb5() {
    let Some((mut mcu, _rx)) = boot() else {
        eprintln!("SKIP: demo.hex missing");
        return;
    };
    let edges: Arc<Mutex<u32>> = Arc::default();
    let counter = edges.clone();
    mcu.on_pin_change(Box::new(move |pin: PinId, _high, _cycle| {
        if pin.port == 'B' && pin.bit == 5 {
            *counter.lock().unwrap() += 1;
        }
    }));
    // Toggle period is 100ms; in 1.05s expect ~10 toggles (allow slack for
    // ADC-loop timing).
    mcu.run_millis(1050).unwrap();
    let n = *edges.lock().unwrap();
    assert!((6..=14).contains(&n), "expected ~10 PB5 toggles, got {n}");
}

#[test]
fn adc_voltage_readback() {
    let Some((mut mcu, rx)) = boot() else {
        eprintln!("SKIP: demo.hex missing");
        return;
    };
    mcu.run_millis(30).unwrap();
    mcu.set_analog_in(0, 2.5);
    mcu.run_millis(30).unwrap();
    rx.lock().unwrap().clear();
    mcu.uart_write(b"v");
    mcu.run_millis(30).unwrap();
    let reply = uart_string(&rx);
    let mv: u32 = reply
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .trim()
        .trim_end_matches("mV")
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("unparseable voltage reply {reply:?}"));
    assert!(
        (2300..=2700).contains(&mv),
        "expected ~2500mV for 2.5V input, got {mv} ({reply:?})"
    );
}
