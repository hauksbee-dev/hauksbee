//! Output sinks: a VCD logger that records digital transitions on chosen nets.
//!
//! The [`VcdSink`] samples a set of nets after every analog solve, decides each
//! one's logic level with thresholds + hysteresis, and records a timestamped
//! change whenever a net's level flips. On [`VcdSink::write`] it emits a
//! Value Change Dump (IEEE 1364) that gtkwave and other waveform viewers open
//! directly. It composes with everything: any net any other peripheral or the
//! firmware drives can be logged without touching them.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use galvani_ir::NodeId;

use super::{Peripheral, TickCtx};

/// Logic-threshold pair for deciding a net's level with hysteresis.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub vih: f64,
    pub vil: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        // 5 V CMOS-ish midpoint with wide hysteresis; works for 3.3 V too.
        Thresholds { vih: 2.0, vil: 0.8 }
    }
}

impl Thresholds {
    fn decide(&self, v: f64, prev: bool) -> bool {
        if v >= self.vih {
            true
        } else if v <= self.vil {
            false
        } else {
            prev
        }
    }
}

/// One logged net: its node, VCD identifier code, and last decided level.
struct LoggedNet {
    name: String,
    node: NodeId,
    code: char,
    level: bool,
    initialized: bool,
}

/// A recorded value change: (time_ps, code, level).
struct Change {
    t_ps: u64,
    code: char,
    level: bool,
}

/// VCD transition logger over a chosen set of nets.
pub struct VcdSink {
    id: String,
    nets: Vec<LoggedNet>,
    thresholds: Thresholds,
    changes: Vec<Change>,
    /// VCD timescale denominator: we record in picoseconds.
    last_t_ps: u64,
    path: Option<PathBuf>,
}

impl VcdSink {
    /// Create a sink logging `nets` (name + resolved node), with default
    /// thresholds. `path` is where [`VcdSink::write`] dumps the trace.
    pub fn new(id: &str, nets: Vec<(String, NodeId)>, path: Option<PathBuf>) -> Self {
        let mut codes = ('!'..='~').filter(|c| *c != '"' && *c != '$');
        let logged = nets
            .into_iter()
            .map(|(name, node)| LoggedNet {
                name,
                node,
                code: codes.next().unwrap_or('!'),
                level: false,
                initialized: false,
            })
            .collect();
        VcdSink {
            id: id.to_string(),
            nets: logged,
            thresholds: Thresholds::default(),
            changes: Vec::new(),
            last_t_ps: 0,
            path,
        }
    }

    /// Override the logic thresholds.
    pub fn with_thresholds(mut self, t: Thresholds) -> Self {
        self.thresholds = t;
        self
    }

    /// Number of recorded transitions (for assertions / tests).
    pub fn transition_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| c.t_ps > 0 || c.level)
            .count()
    }

    /// Transitions recorded for a named net.
    pub fn transitions_for(&self, net: &str) -> usize {
        let Some(code) = self.nets.iter().find(|n| n.name == net).map(|n| n.code) else {
            return 0;
        };
        // Count changes after the initial t=0 dump.
        let first = self
            .nets
            .iter()
            .find(|n| n.name == net)
            .map(|n| n.code)
            .unwrap();
        let _ = first;
        self.changes
            .iter()
            .filter(|c| c.code == code)
            .count()
            .saturating_sub(1) // drop the initial value dump
    }

    /// Render the VCD document as a string (gtkwave-compatible).
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("$date galvani $end\n");
        s.push_str("$version galvani-vcd-sink $end\n");
        s.push_str("$timescale 1ps $end\n");
        s.push_str("$scope module galvani $end\n");
        for n in &self.nets {
            // 1-bit wire per net.
            s.push_str(&format!("$var wire 1 {} {} $end\n", n.code, sanitize(&n.name)));
        }
        s.push_str("$upscope $end\n");
        s.push_str("$enddefinitions $end\n");

        // Group changes by timestamp in order.
        let mut last_t = u64::MAX;
        for c in &self.changes {
            if c.t_ps != last_t {
                s.push_str(&format!("#{}\n", c.t_ps));
                last_t = c.t_ps;
            }
            s.push_str(&format!("{}{}\n", if c.level { '1' } else { '0' }, c.code));
        }
        s
    }

    /// Write the VCD to the configured path (no-op if none set).
    pub fn write(&self) -> std::io::Result<()> {
        if let Some(p) = &self.path {
            let mut f = std::fs::File::create(p)?;
            f.write_all(self.render().as_bytes())?;
        }
        Ok(())
    }

    /// Write the VCD to an explicit path.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let mut f = std::fs::File::create(path)?;
        f.write_all(self.render().as_bytes())?;
        Ok(())
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

impl Peripheral for VcdSink {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "vcd_sink"
    }

    fn post_solve(&mut self, ctx: &mut TickCtx) {
        // Sample at the end of this chunk.
        let t_ps = ((ctx.t + ctx.dt) * 1e12).round() as u64;
        self.last_t_ps = t_ps;
        for n in &mut self.nets {
            let v = ctx.volts(n.node);
            let level = self.thresholds.decide(v, n.level);
            if !n.initialized {
                // Dump the initial value at the net's first observation.
                n.level = level;
                n.initialized = true;
                self.changes.push(Change {
                    t_ps: 0,
                    code: n.code,
                    level,
                });
            } else if level != n.level {
                n.level = level;
                self.changes.push(Change {
                    t_ps,
                    code: n.code,
                    level,
                });
            }
        }
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("nets".into(), self.nets.len() as f64);
        m.insert("transitions".into(), self.transition_count() as f64);
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use galvani_ir::Circuit;

    #[test]
    fn vcd_records_transitions_and_renders() {
        let mut c = Circuit::new();
        let n = c.node("CLK");
        let mut sink = VcdSink::new("VCD", vec![("CLK".into(), n)], None);

        // Drive a square wave by feeding alternating voltages.
        let mut volts = vec![0.0; c.node_count()];
        for step in 0..6 {
            volts[n.0 as usize] = if step % 2 == 0 { 0.0 } else { 5.0 };
            let mut ctx = TickCtx {
                circuit: &mut c,
                node_volts: &volts,
                t: step as f64 * 1e-4,
                dt: 1e-4,
            };
            sink.post_solve(&mut ctx);
        }
        // 6 samples alternating low/high -> 5 transitions after the initial.
        assert_eq!(sink.transitions_for("CLK"), 5, "expected 5 CLK edges");
        let vcd = sink.render();
        assert!(vcd.contains("$timescale 1ps"), "has timescale");
        assert!(vcd.contains("$var wire 1"), "declares the net");
        assert!(vcd.contains("$enddefinitions"), "well-formed header");
    }
}
