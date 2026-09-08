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

use hauksbee_ir::NodeId;

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
    code: String,
    level: bool,
    initialized: bool,
}

/// A recorded value change: (time_ps, code, level).
struct Change {
    t_ps: u64,
    code: String,
    level: bool,
}

/// Map a net index to a unique VCD identifier code. VCD identifier codes are
/// strings of printable ASCII (33..=126); a single char only spans 92 usable
/// glyphs (we exclude `"` and `$`, which delimit VCD tokens), so nets past the
/// 92nd MUST roll over to multi-character codes. This is a bijective base-92
/// numeral over that alphabet: indices 0..92 are one char, 92..(92+92²) are two,
/// and so on, every index gets a distinct code, none ever collides on `!`.
fn vcd_code(index: usize) -> String {
    const EXCLUDED: [char; 2] = ['"', '$'];
    let alphabet: Vec<char> = ('!'..='~').filter(|c| !EXCLUDED.contains(c)).collect();
    let k = alphabet.len();
    let mut n = index;
    let mut out = Vec::new();
    loop {
        out.push(alphabet[n % k]);
        n /= k;
        if n == 0 {
            break;
        }
        n -= 1; // bijective: no leading "zero" digit
    }
    out.iter().rev().collect()
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
        let logged = nets
            .into_iter()
            .enumerate()
            .map(|(i, (name, node))| LoggedNet {
                name,
                node,
                code: vcd_code(i),
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
        // The initial value dump every net emits at t=0 is not a transition,
        // regardless of its level, exclude it by timestamp only. The old
        // `|| c.level` term wrongly counted the t=0 dump for a net that powers
        // up HIGH, over-counting by one (inconsistent with `transitions_for`,
        // which drops the initial dump via `saturating_sub(1)`).
        self.changes.iter().filter(|c| c.t_ps > 0).count()
    }

    /// Transitions recorded for a named net.
    pub fn transitions_for(&self, net: &str) -> usize {
        let Some(code) = self
            .nets
            .iter()
            .find(|n| n.name == net)
            .map(|n| n.code.clone())
        else {
            return 0;
        };
        // Count changes after the initial t=0 dump.
        self.changes
            .iter()
            .filter(|c| c.code == code)
            .count()
            .saturating_sub(1) // drop the initial value dump
    }

    /// Render the VCD document as a string (gtkwave-compatible).
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("$date hauksbee $end\n");
        s.push_str("$version hauksbee-vcd-sink $end\n");
        s.push_str("$timescale 1ps $end\n");
        s.push_str("$scope module hauksbee $end\n");
        for n in &self.nets {
            // 1-bit wire per net.
            s.push_str(&format!(
                "$var wire 1 {} {} $end\n",
                n.code,
                sanitize(&n.name)
            ));
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
    /// Create the output's parent directory if it is not there.
    ///
    /// Naming an output under a directory that does not exist yet is an
    /// ordinary thing to do, and the bare failure is "No such file or
    /// directory (os error 2)" with no mention of which path or why. A run
    /// that has already done the simulation should not throw the result away
    /// over a missing folder.
    fn ensure_parent(path: &Path) -> std::io::Result<()> {
        match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => std::fs::create_dir_all(dir),
            _ => Ok(()),
        }
    }

    pub fn write(&self) -> std::io::Result<()> {
        if let Some(p) = &self.path {
            Self::ensure_parent(p)?;
            let mut f = std::fs::File::create(p)?;
            f.write_all(self.render().as_bytes())?;
        }
        Ok(())
    }

    /// Write the VCD to an explicit path.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        Self::ensure_parent(path)?;
        let mut f = std::fs::File::create(path)?;
        f.write_all(self.render().as_bytes())?;
        Ok(())
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
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
                    code: n.code.clone(),
                    level,
                });
            } else if level != n.level {
                n.level = level;
                self.changes.push(Change {
                    t_ps,
                    code: n.code.clone(),
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
    use hauksbee_ir::Circuit;

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

    #[test]
    fn transition_count_excludes_the_t0_dump_even_when_high() {
        // A net that powers up HIGH emits its initial value dump at t=0 with
        // level=HIGH. That dump is NOT a transition; transition_count must
        // exclude it (counting it via `|| c.level` over-counts by 1 and
        // disagrees with transitions_for).
        let mut c = Circuit::new();
        let n = c.node("PWR");
        let mut sink = VcdSink::new("VCD", vec![("PWR".into(), n)], None);
        let levels = [5.0, 0.0, 5.0, 0.0]; // HIGH at t=0, then 3 edges
        let mut volts = vec![0.0; c.node_count()];
        for (step, &v) in levels.iter().enumerate() {
            volts[n.0 as usize] = v;
            let mut ctx = TickCtx {
                circuit: &mut c,
                node_volts: &volts,
                t: step as f64 * 1e-4,
                dt: 1e-4,
            };
            sink.post_solve(&mut ctx);
        }
        assert_eq!(
            sink.transitions_for("PWR"),
            3,
            "per-net count drops the initial dump"
        );
        assert_eq!(
            sink.transition_count(),
            3,
            "aggregate must match, not over-count the HIGH t=0 dump"
        );
    }

    #[test]
    fn vcd_codes_stay_unique_past_the_single_char_alphabet() {
        // The usable single-char VCD alphabet is 92 glyphs. Before the fix every
        // net past the 92nd fell back to '!', colliding all their value changes
        // onto the first net's identifier and corrupting the trace. Codes must
        // stay pairwise-distinct for any count of nets.
        const N: usize = 500; // well past 92, into the two-char range
        let codes: Vec<String> = (0..N).map(vcd_code).collect();
        let unique: std::collections::HashSet<&String> = codes.iter().collect();
        assert_eq!(
            unique.len(),
            N,
            "every net index must get a distinct VCD code"
        );
        // First 92 are single-char; the 93rd rolls over to two chars.
        assert_eq!(codes[0].len(), 1);
        assert_eq!(codes[91].len(), 1);
        assert_eq!(codes[92].len(), 2);
        // And the allocator wired into a sink never reuses '!' for the 93rd net.
        let mut c = Circuit::new();
        let nets: Vec<(String, NodeId)> = (0..100)
            .map(|i| (format!("N{i}"), c.node(&format!("N{i}"))))
            .collect();
        let sink = VcdSink::new("VCD", nets, None);
        let doc = sink.render();
        // The 93rd net's declared code must be the two-char rollover, not '!'.
        assert!(doc.contains(&format!("$var wire 1 {} N92 $end", vcd_code(92))));
    }
}
