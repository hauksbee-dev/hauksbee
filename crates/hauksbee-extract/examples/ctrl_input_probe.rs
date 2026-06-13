//! Measurement harness for the "undriven control input at power-up" candidate
//! check, kept so the decisive NEGATIVE engineering result in
//! `docs/KNOWN_FAULTS_VALIDATION.md` is auditable and re-runnable.
//!
//! Pattern: a non-rail, non-ground net carrying a dedicated control-pin function
//! (OE / EN / RST / CS / SHDN) with NO pull resistor on the net reaching a rail
//! or ground (no defined level), where the only active driver is an MCU/module
//! GPIO (Hi-Z at power-up / deep sleep). This is the structure of two documented
//! faults (Watchy #14 RES#, ZSWatch DevKit #123 DISPLAY-EN). The harness exists
//! to show that the IDENTICAL structure also appears on benign, shipped designs
//! (DISPLAY-CS, DISPLAY-RST, VIB-EN, power-gates), so a structural check on it
//! would manufacture false positives. Run it on faulty boards (it fires on the
//! real fault) and on the clean corpus (it fires on the benign twins):
//!
//!   cargo run -p hauksbee-extract --example ctrl_input_probe -- <board-file>
//!
//! It is a probe, not a check: it deliberately does NOT live in `net_lint()`.

use hauksbee_extract::ExtractedBoard;

fn norm(name: &str) -> String {
    name.trim().rsplit('/').next().unwrap_or(name).trim().to_ascii_uppercase()
}

fn is_ground(name: &str) -> bool {
    let n = norm(name);
    matches!(n.as_str(), "GND" | "GNDA" | "GNDD" | "AGND" | "DGND" | "PGND" | "VSS" | "0")
        || n.starts_with("GND")
}

fn is_rail(name: &str) -> bool {
    let n = norm(name);
    n.starts_with('+')
        || n.contains("3V3")
        || n.contains("3.3V")
        || n.contains("5V")
        || n.contains("1V8")
        || n.contains("VCC")
        || n.contains("VDD")
        || n.contains("VBAT")
        || n.contains("VSYS")
        || n.contains("VBUS")
}

/// Dedicated control role from a pin function name (mirrors netlint's strict
/// `control_role`: rejects multiplexed/signal names).
fn control_role(function: &str) -> Option<&'static str> {
    let f = function.trim().to_ascii_uppercase();
    if f.is_empty() {
        return None;
    }
    const SIGNAL_KEYWORDS: [&str; 12] = [
        "GPIO", "EMAC", "RMII", "TX_EN", "RX_EN", "CLKEN", "VSPI", "HSPI", "UART", "PWM", "SENSE",
        "OPEN",
    ];
    if SIGNAL_KEYWORDS.iter().any(|k| f.contains(k)) {
        return None;
    }
    if f.contains('/') && !f.starts_with('/') {
        return None;
    }
    let core: String =
        f.trim_start_matches('~').trim_start_matches('/').replace(['~', '{', '}', '/', '#'], "");
    let core = core.trim_start_matches('N').to_string();
    let core = core.trim_end_matches("_N").to_string();
    let c = core.trim();
    match c {
        "EN" | "ENABLE" | "CE" | "CEN" | "SHDN" | "SHUTDOWN" | "NSHDN" => Some("enable"),
        "RST" | "RESET" | "MR" | "RESE" => Some("reset"),
        "CS" | "SS" | "NCS" | "NSS" | "CSB" => Some("chip-select"),
        "OE" | "NOE" => Some("output-enable"),
        _ => {
            let head = c.split('_').next().unwrap_or(c);
            match head {
                "EN" => Some("enable"),
                "RST" | "RESET" => Some("reset"),
                _ => None,
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: ctrl_input_probe <board-file>");
    let text = std::fs::read_to_string(&path)?;
    let board = if path.ends_with(".kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(std::path::Path::new(&path))?
    } else {
        ExtractedBoard::from_auto(&text)?
    };

    let mut hits = 0;
    for net in &board.nets {
        if net.id == 0 || net.name.starts_with("unconnected-") {
            continue;
        }
        if is_ground(&net.name) || is_rail(&net.name) {
            continue;
        }
        let members = board.net_members(net.id);
        let ctrl: Vec<(&str, &str)> = members
            .iter()
            .filter_map(|(c, p)| control_role(&p.function).map(|r| (c.reference.as_str(), r)))
            .collect();
        if ctrl.is_empty() {
            continue;
        }
        // Pull resistor on this net reaching a rail or ground?
        let has_pull = members.iter().any(|(c, _)| {
            let r = c.reference.to_ascii_uppercase();
            let is_r = r.starts_with('R')
                && !r.starts_with("RV")
                && !r.starts_with("RN")
                && !r.starts_with("RT");
            is_r && c.pins.iter().any(|op| {
                op.net
                    .filter(|id| *id != net.id)
                    .and_then(|id| board.net(id))
                    .map(|on| is_rail(&on.name) || is_ground(&on.name))
                    .unwrap_or(false)
            })
        });
        if has_pull {
            continue;
        }
        let mut drivers = Vec::new();
        for (c, p) in &members {
            if control_role(&p.function).is_some() {
                continue;
            }
            let r = c.reference.to_ascii_uppercase();
            let passive = r.starts_with('R')
                || r.starts_with('C')
                || r.starts_with("TP")
                || r.starts_with('J')
                || r.starts_with("CN")
                || r.starts_with('L');
            if passive {
                continue;
            }
            drivers.push(format!("{}.{}={}", c.reference, p.number, p.function));
        }
        hits += 1;
        println!(
            "  net '{}' ctrl={:?} drivers={:?} members={}",
            net.name,
            ctrl,
            drivers,
            members.len()
        );
    }
    println!("== {hits} candidate control-input net(s) in {path}");
    Ok(())
}
