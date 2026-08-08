//! Generic evaluator for declarative digital logic (`[models.logic]`).
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/digital.md
//! (the digital-domain essay) and docs/how-and-why/hauksbee-models/logic_spec.md
//! (the format and the byte-exact migration record).
//!
//! One [`LogicComponent`] covers every digital part: a part's behaviour
//! arrives as data (a validated
//! [`hauksbee_models::logic_spec::Logic`] block) and is COMPILED once at bind
//! time into pin index tables, `u64` register state, and evalexpr operator
//! trees, then evaluated per tick with no parsing and no per-tick
//! allocation of program structure. Ticks happen on every simulated edge in
//! the replay paths, so bind-time compilation is a real performance
//! requirement, not a nicety.
//!
//! ## Expression compilation
//!
//! The spec's boolean grammar (digit-led pin names like `1a`, `!`/`&`/`^`/`|`,
//! `reg[bit]` references) is not evalexpr syntax, so the validated
//! [`LogicExpr`] AST is lowered into a fully parenthesized evalexpr source
//! string over sanitized boolean variables (`v_<name>` for pins/outputs,
//! `b_<reg>_<bit>` for register bits; full parenthesization makes operator
//! precedence a non-issue) and compiled with `build_operator_tree`; the same
//! pattern `register_map.rs` uses for sensor value expressions.
//!
//! ## Evaluation order and combinational cycles
//!
//! Outputs are evaluated in dependency (topological) order. Outputs forming a
//! strongly connected component, a genuine combinational cycle, e.g. the
//! cross-coupled NOR SR latch, are resolved by Gauss–Seidel fixpoint
//! iteration in `outputs`-declaration order (the spec's declared resolution
//! order), seeded from the previous stable levels (`init` at power-on).
//!
//! The iteration bound is [`COMB_FIXPOINT_BOUND`] sweeps. The bound is not a
//! correctness knob: at compile time every SCC is checked EXHAUSTIVELY, all
//! `2^m` assignments of its external inputs crossed with all `2^k` seed
//! states of its members, and a spec whose cycle fails to settle within the
//! bound for any combination is REFUSED with a named error and a concrete
//! witness (`y = !y` is the canonical refusal). A cycle too wide to enumerate
//! (`m + k >` [`CONVERGENCE_ENUM_CAP`]) is likewise refused rather than
//! shipped unverified. Runtime non-convergence is therefore unreachable for a
//! compiled spec; the runtime bound remains as defense in depth and screams
//! on stderr if it ever trips.
//!
//! Gauss–Seidel (not Jacobi) is deliberate: sequential sweeps in declared
//! order settle every physical latch deterministically from any seed, whereas
//! simultaneous updates oscillate forever from the symmetric seeds (the
//! classic SR metastability). The declared order IS the tie-break the plan
//! calls "a declared resolution order".
//!
//! ## Register semantics
//!
//! See the `logic_spec` module docs for the full contract. In short: per tick
//! every register computes its next value from the PRE-tick state with
//! priority reset > async load > clock edge (matching the 74HC595 SRCLR and
//! 74HC165 PL datasheet rows), then all registers commit simultaneously,
//! tied shift+latch clocks behave like real silicon (store one clock behind
//! shift).
//!
//! ## Unwired pins
//!
//! An unwired input reads LOW in expressions and as data/clock. An unwired
//! CONTROL pin reads the level that keeps the part operating normally:
//! resets and loads released, clocks enabled, tri-states driving. (A board
//! that ties these does so in copper; a spec input left unwired must not
//! freeze the part.) So an unwired SRCLR_n reads released and an unwired
//! OE_n reads enabled. The rule is uniform: an unwired 74HC165 PL_n also
//! reads released rather than LOW, so parallel load is NOT permanently
//! transparent on an unwired PL_n. A permanently transparent load is a
//! modeling artifact rather than silicon behaviour, so a board that wants it
//! must tie PL_n low in copper as the real part requires.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, DefaultNumericTypes, HashMapContext,
    Node as EvalNode, Value,
};
use hauksbee_models::logic_spec::{Edge, Level, Logic, LogicExpr, LogicSpecError, RegisterOp};

/// Fixpoint sweep bound for combinational cycles. Part of the semantics
/// contract: the compile-time exhaustive check verifies every SCC settles
/// within this many Gauss–Seidel sweeps for every input/seed combination.
/// Physical latch structures settle in ≤ 2 sweeps; 16 leaves generous slack
/// for legitimate wider cycles without letting a pathological spec spin.
pub const COMB_FIXPOINT_BOUND: usize = 16;

/// Maximum `external inputs + cycle members` the exhaustive convergence check
/// will enumerate (2^cap evaluations). A cycle wider than this is refused
/// rather than shipped unverified, refuse-rather-than-fake.
pub const CONVERGENCE_ENUM_CAP: u32 = 12;

/// A named logic-compilation failure (everything `logic_spec` validation
/// cannot know statically, plus the pass-through of validation itself).
#[derive(Debug, thiserror::Error)]
pub enum LogicCompileError {
    #[error("logic spec: {0}")]
    Spec(#[from] LogicSpecError),

    #[error(
        "combinational cycle through {outputs:?} does not converge within \
         {bound} fixpoint sweeps for {witness}; a comb network that cannot \
         settle is refused, not approximated"
    )]
    NonConvergent {
        outputs: Vec<String>,
        bound: usize,
        witness: String,
    },

    #[error(
        "combinational cycle through {outputs:?} has {external} external \
         input(s) and {members} member(s): {total} > {cap} enumeration bits, \
         too wide to verify convergence exhaustively; refused rather than \
         shipped unverified"
    )]
    TooWideToVerify {
        outputs: Vec<String>,
        external: u32,
        members: u32,
        total: u32,
        cap: u32,
    },

    /// The AST-to-evalexpr lowering produced something evalexpr rejected.
    /// Structurally unreachable (the lowering emits only boolean operators
    /// over identifiers); kept loud instead of unwrapped.
    #[error("internal: lowered expression for '{output}' failed to compile: {message}")]
    Lowering { output: String, message: String },
}

/// Where a register's serial/parallel data comes from (resolved indices).
#[derive(Debug, Clone, Copy)]
enum DataSrc {
    Pin(usize),
    Reg(usize),
}

/// One compiled register.
#[derive(Debug, Clone)]
struct CompReg {
    name: String,
    bits: u32,
    mask: u64,
    state: u64,
    /// (input pin index, edge), plus the previous clock level for edge detect.
    clock: Option<(usize, Edge)>,
    prev_clock: bool,
    /// (input pin index, active level, reset value), first active wins.
    resets: Vec<(usize, Level, u64)>,
    op: Option<RegisterOp>,
    data_in: Option<DataSrc>,
    /// (input pin index, active level, data pin index per bit).
    load: Option<(usize, Level, Vec<usize>)>,
    clock_enable: Option<(usize, Level)>,
}

/// One compiled combinational output.
#[derive(Debug)]
struct CompComb {
    /// Index into `outputs`.
    out: usize,
    node: EvalNode<DefaultNumericTypes>,
}

/// One evaluation-plan step: a single acyclic output, or a strongly connected
/// component resolved by fixpoint iteration (members in declaration order).
#[derive(Debug)]
enum PlanStep {
    Single(usize),
    Scc(Vec<usize>),
}

/// A compiled tri-state group.
#[derive(Debug, Clone)]
struct CompTristate {
    /// Output indices gated by this group.
    outputs: Vec<usize>,
    /// Input pin index of the enable.
    enable: usize,
    active: Level,
}

/// One compiled addressable memory array. Storage is shared so an
/// edge-synchronous bus responder can use the same cells as the ordinary
/// tick evaluator; there is one source of truth even when firmware reads and
/// writes between analogue solve boundaries.
#[derive(Debug)]
struct CompMemory {
    name: String,
    mask: u64,
    address: Vec<usize>,
    writes: Vec<CompMemoryWrite>,
    read_gates: Vec<(usize, Level)>,
    data_in_names: Vec<String>,
    data_in_levels: Vec<bool>,
    data_out: Vec<usize>,
    byte_load_timeout_s: Option<f64>,
    program_time_s: Option<f64>,
    storage: Arc<Mutex<ParallelMemoryStorage>>,
}

#[derive(Debug, Clone)]
struct CompMemoryWrite {
    pin: usize,
    edge: Edge,
    prev: bool,
    gates: Vec<(usize, Level)>,
}

#[derive(Debug)]
struct SoftwareProtectionState {
    enabled: bool,
    enable: Box<[(usize, u64)]>,
    disable: Box<[(usize, u64)]>,
    page_words: usize,
    /// Candidate command writes are held until they either complete a command
    /// (and are consumed) or diverge. While unprotected, a divergent prefix is
    /// ordinary data and must be committed; while protected it is ignored.
    command_buffer: Vec<(usize, u64)>,
    /// The enable sequence arms one protected byte/page program operation.
    /// A read, a page crossing, or `page_words` physical writes closes it.
    write_window: Option<ProtectedWriteWindow>,
}

#[derive(Debug)]
struct ProtectedWriteWindow {
    page: Option<usize>,
    remaining: usize,
}

#[derive(Debug)]
struct ParallelMemoryStorage {
    cells: Box<[u64]>,
    protection: Option<SoftwareProtectionState>,
    page_words: usize,
    timed_page: Option<TimedPage>,
    busy: Option<TimedProgram>,
    last_timed_write_cycle: Option<u64>,
}

#[derive(Debug)]
struct TimedPage {
    page: usize,
    values: Vec<(usize, u64)>,
    load_count: usize,
    last_cycle: u64,
    poll_value: u64,
}

#[derive(Debug)]
struct TimedProgram {
    until_cycle: u64,
    values: Vec<(usize, u64)>,
    poll_value: u64,
    toggle: bool,
}

impl ParallelMemoryStorage {
    fn advance_timed(&mut self, cycle: u64) {
        let complete = self
            .busy
            .as_ref()
            .is_some_and(|busy| cycle >= busy.until_cycle);
        if complete {
            let busy = self.busy.take().expect("completion checked above");
            for (address, value) in busy.values {
                self.cells[address] = value;
            }
        }
    }

    fn begin_program(&mut self, program_cycles: u64) {
        let Some(page) = self.timed_page.take() else {
            return;
        };
        self.busy = Some(TimedProgram {
            until_cycle: page.last_cycle.saturating_add(program_cycles),
            values: page.values,
            poll_value: page.poll_value,
            toggle: false,
        });
    }

    fn timed_write(
        &mut self,
        address: usize,
        value: u64,
        mask: u64,
        cycle: u64,
        timeout_cycles: u64,
        program_cycles: u64,
    ) -> bool {
        if address >= self.cells.len() {
            return false;
        }
        // HAUKSBEE_EEPROM_DEBUG diagnostics: env-gated, silent by default. Kept
        // for the documented residual positive-control flake (a page-load
        // occasionally converts to a program cycle right after the previous
        // page's poll completes; see the NEP acceptance test's module header).
        if std::env::var_os("HAUKSBEE_EEPROM_DEBUG").is_some() {
            eprintln!(
                "EEPROM-DEBUG write ENTER addr {address:#06x} val {value:#04x} cycle {cycle} busy {:?} pending {:?}",
                self.busy.as_ref().map(|b| b.until_cycle),
                self.timed_page
                    .as_ref()
                    .map(|p| (p.page, p.last_cycle, p.load_count))
            );
        }
        self.advance_timed(cycle);
        if self.busy.is_some() {
            // A write during the internal program cycle is inhibited.
            // HAUKSBEE_EEPROM_DEBUG: name every swallowed write.
            if std::env::var_os("HAUKSBEE_EEPROM_DEBUG").is_some() {
                eprintln!(
                    "EEPROM-DEBUG write INHIBITED addr {address:#06x} val {value:#04x} cycle {cycle} busy_until {:?}",
                    self.busy.as_ref().map(|b| b.until_cycle)
                );
            }
            return true;
        }
        let page = address / self.page_words;
        let closes_previous = self.timed_page.as_ref().is_some_and(|pending| {
            page != pending.page
                || cycle < pending.last_cycle
                || cycle - pending.last_cycle > timeout_cycles
                || pending.load_count >= self.page_words
        });
        if closes_previous {
            self.begin_program(program_cycles);
            self.advance_timed(cycle);
            if self.busy.is_some() {
                return true;
            }
        }

        let value = value & mask;
        // Run the protection protocol, but move any resulting cell changes
        // into the page latch until the internal program cycle completes.
        let mut watched = vec![address];
        if let Some(protection) = &self.protection {
            watched.extend(
                protection
                    .command_buffer
                    .iter()
                    .map(|(address, _)| *address),
            );
        }
        watched.sort_unstable();
        watched.dedup();
        let before: Vec<(usize, u64)> = watched
            .iter()
            .map(|&candidate| (candidate, self.cells[candidate]))
            .collect();
        let protection_was_enabled = self.protection.as_ref().is_some_and(|p| p.enabled);
        let accepted = self.write(address, value, mask, false);
        self.last_timed_write_cycle = Some(cycle);
        let mut changes = Vec::new();
        for (candidate, old) in before {
            let new = self.cells[candidate];
            if new != old {
                changes.push((candidate, new));
                self.cells[candidate] = old;
            }
        }
        if changes.is_empty() {
            let blocked_attempt = self.protection.as_ref().is_some_and(|protection| {
                protection.enabled
                    && protection.write_window.is_none()
                    && protection.command_buffer.is_empty()
            });
            if blocked_attempt {
                self.busy = Some(TimedProgram {
                    until_cycle: cycle.saturating_add(program_cycles),
                    values: Vec::new(),
                    poll_value: value,
                    toggle: false,
                });
            } else if protection_was_enabled && self.protection.as_ref().is_some_and(|p| !p.enabled)
            {
                // The final SDP-disable command itself has a write-cycle
                // recovery interval even though it changes no array byte.
                self.busy = Some(TimedProgram {
                    until_cycle: cycle.saturating_add(program_cycles),
                    values: Vec::new(),
                    poll_value: value,
                    toggle: false,
                });
            }
            return accepted;
        }
        let pending = self.timed_page.get_or_insert_with(|| TimedPage {
            page,
            values: Vec::new(),
            load_count: 0,
            last_cycle: cycle,
            poll_value: value,
        });
        for (changed_address, changed_value) in changes {
            if let Some(existing) = pending
                .values
                .iter_mut()
                .find(|(staged_address, _)| *staged_address == changed_address)
            {
                existing.1 = changed_value;
            } else {
                pending.values.push((changed_address, changed_value));
            }
        }
        pending.last_cycle = cycle;
        pending.load_count += 1;
        pending.poll_value = value;
        accepted
    }

    fn timed_read(
        &mut self,
        address: usize,
        mask: u64,
        cycle: u64,
        program_cycles: u64,
    ) -> Option<u64> {
        if address >= self.cells.len() {
            return None;
        }
        // An incomplete command-looking prefix is disambiguated by a read.
        // Unprotected prefixes are ordinary bytes and therefore still need a
        // program interval; protected prefixes are rejected attempts which
        // enter busy without mutating the array.
        if self.timed_page.is_none()
            && self
                .protection
                .as_ref()
                .is_some_and(|p| !p.command_buffer.is_empty())
        {
            let protected = self.protection.as_ref().is_some_and(|p| p.enabled);
            let pending: Vec<usize> = self
                .protection
                .as_ref()
                .into_iter()
                .flat_map(|p| p.command_buffer.iter().map(|(address, _)| *address))
                .collect();
            let before: Vec<(usize, u64)> = pending
                .iter()
                .map(|&candidate| (candidate, self.cells[candidate]))
                .collect();
            let poll_value = self
                .protection
                .as_ref()
                .and_then(|p| p.command_buffer.last().map(|(_, value)| *value))
                .unwrap_or(0);
            self.finish_write_period();
            let mut values = Vec::new();
            for (candidate, old) in before {
                let new = self.cells[candidate];
                if new != old {
                    values.push((candidate, new));
                    self.cells[candidate] = old;
                }
            }
            if protected || !values.is_empty() {
                self.busy = Some(TimedProgram {
                    until_cycle: self
                        .last_timed_write_cycle
                        .unwrap_or(cycle)
                        .saturating_add(program_cycles),
                    values,
                    poll_value,
                    toggle: false,
                });
            }
        }
        if self.timed_page.is_some() {
            self.begin_program(program_cycles);
        }
        self.advance_timed(cycle);
        if let Some(busy) = &mut self.busy {
            let mut value = self.cells[address] & mask;
            // AT28 data polling: I/O7 is the complement of the last loaded
            // data bit and I/O6 toggles on consecutive reads.
            value = (value & !(1 << 7)) | ((!busy.poll_value) & (1 << 7));
            busy.toggle = !busy.toggle;
            value = (value & !(1 << 6)) | (u64::from(busy.toggle) << 6);
            // HAUKSBEE_EEPROM_DEBUG: trace busy poll answers.
            if std::env::var_os("HAUKSBEE_EEPROM_DEBUG").is_some() {
                eprintln!(
                    "EEPROM-DEBUG read POLL addr {address:#06x} -> {:#04x} cycle {cycle} busy_until {} poll_value {:#04x}",
                    value & mask,
                    busy.until_cycle,
                    busy.poll_value
                );
            }
            return Some(value & mask);
        }
        // HAUKSBEE_EEPROM_DEBUG: trace settled reads too (they are what a
        // wrongly-satisfied poll consumed).
        if std::env::var_os("HAUKSBEE_EEPROM_DEBUG").is_some() {
            eprintln!(
                "EEPROM-DEBUG read SETTLED addr {address:#06x} -> {:?} cycle {cycle}",
                self.cells.get(address).map(|v| v & mask)
            );
        }
        self.read(address, mask)
    }

    fn finish_write_period(&mut self) {
        if let Some(protection) = &mut self.protection {
            protection.write_window = None;
            if !protection.enabled {
                for (pending_address, pending_value) in
                    std::mem::take(&mut protection.command_buffer)
                {
                    self.cells[pending_address] = pending_value;
                }
            } else {
                protection.command_buffer.clear();
            }
        }
    }

    fn read(&mut self, address: usize, mask: u64) -> Option<u64> {
        // Reading terminates a page-load period. It also disambiguates an
        // incomplete command prefix: in the unprotected state those bus
        // writes were ordinary data; in the protected state they were
        // rejected attempts and remain uncommitted.
        self.finish_write_period();
        self.cells.get(address).copied().map(|v| v & mask)
    }

    /// Apply one qualified physical write. Returns false only for an address
    /// outside the array; a write blocked by enabled protection is a valid bus
    /// transaction and therefore returns true.
    fn write(&mut self, address: usize, value: u64, mask: u64, gap_exceeded: bool) -> bool {
        if address >= self.cells.len() {
            return false;
        }
        if gap_exceeded {
            self.finish_write_period();
        }
        let value = value & mask;
        let Some(protection) = &mut self.protection else {
            self.cells[address] = value;
            return true;
        };

        let write = (address, value);

        // Once armed, the protected program operation accepts one byte or one
        // same-page load of up to `page_words` writes. The AT28C256's command
        // bytes are followed by this data phase; treating protection as a
        // blanket write block here would reject the datasheet's own algorithm.
        if let Some(window) = &mut protection.write_window {
            let page = address / protection.page_words;
            if window.page.is_none() || window.page == Some(page) {
                window.page = Some(page);
                self.cells[address] = value;
                window.remaining -= 1;
                if window.remaining == 0 {
                    protection.write_window = None;
                }
                return true;
            }
            // A new page needs a new enable sequence while protected.
            protection.write_window = None;
        }

        protection.command_buffer.push(write);
        if protection.command_buffer.as_slice() == protection.enable.as_ref() {
            protection.command_buffer.clear();
            protection.enabled = true;
            protection.write_window = Some(ProtectedWriteWindow {
                page: None,
                remaining: protection.page_words,
            });
            return true;
        }
        if protection.command_buffer.as_slice() == protection.disable.as_ref() {
            protection.command_buffer.clear();
            protection.enabled = false;
            protection.write_window = None;
            return true;
        }
        let is_prefix = |candidate: &[(usize, u64)], sequence: &[(usize, u64)]| {
            candidate.len() < sequence.len() && sequence.starts_with(candidate)
        };
        if is_prefix(&protection.command_buffer, &protection.enable)
            || is_prefix(&protection.command_buffer, &protection.disable)
        {
            return true;
        }

        // The candidate diverged. Preserve a final write that can immediately
        // start another command; everything before it is either ordinary data
        // (unprotected) or a rejected protected write.
        let mut candidate = std::mem::take(&mut protection.command_buffer);
        let restart = candidate.last().is_some_and(|last| {
            protection.enable.first() == Some(last) || protection.disable.first() == Some(last)
        });
        let held = restart.then(|| candidate.pop().expect("restart has a final write"));
        if !protection.enabled {
            for (pending_address, pending_value) in candidate {
                self.cells[pending_address] = pending_value;
            }
        }
        protection.command_buffer.extend(held);
        true
    }
}

/// Cloneable description and shared storage for one compiled parallel-memory
/// port. The scheduler combines these role names with the bound board's nets
/// when it installs an edge-synchronous firmware responder.
#[derive(Debug, Clone)]
pub(crate) struct ParallelMemoryPort {
    pub name: String,
    pub address: Vec<String>,
    pub writes: Vec<ParallelMemoryWritePort>,
    pub read_gates: Vec<(String, Level)>,
    pub data_in: Vec<String>,
    pub data_out: Vec<String>,
    pub byte_load_timeout_s: Option<f64>,
    pub program_time_s: Option<f64>,
    storage: Arc<Mutex<ParallelMemoryStorage>>,
    mask: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ParallelMemoryWritePort {
    pub pin: String,
    pub edge: Edge,
    pub gates: Vec<(String, Level)>,
}

impl ParallelMemoryPort {
    pub fn words(&self) -> usize {
        self.storage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cells
            .len()
    }

    pub fn read(&self, address: usize) -> Option<u64> {
        self.storage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read(address, self.mask)
    }

    pub fn read_at(&self, address: usize, cycle: u64, frequency_hz: u64) -> Option<u64> {
        let program_s = self.program_time_s?;
        let program_cycles = seconds_to_cycles(program_s, frequency_hz);
        self.storage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .timed_read(address, self.mask, cycle, program_cycles)
    }

    #[cfg(test)]
    pub fn write(&self, address: usize, value: u64) -> bool {
        self.storage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(address, value, self.mask, false)
    }

    pub fn write_after_gap(&self, address: usize, value: u64, gap_exceeded: bool) -> bool {
        self.storage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(address, value, self.mask, gap_exceeded)
    }

    pub fn write_at(&self, address: usize, value: u64, cycle: u64, frequency_hz: u64) -> bool {
        let Some(program_s) = self.program_time_s else {
            return self.write_after_gap(address, value, false);
        };
        let timeout_cycles = self
            .byte_load_timeout_s
            .map(|seconds| seconds_to_cycles(seconds, frequency_hz))
            .unwrap_or(0);
        let program_cycles = seconds_to_cycles(program_s, frequency_hz);
        self.storage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .timed_write(
                address,
                value,
                self.mask,
                cycle,
                timeout_cycles,
                program_cycles,
            )
    }
}

fn seconds_to_cycles(seconds: f64, frequency_hz: u64) -> u64 {
    (seconds * frequency_hz.max(1) as f64).ceil().max(1.0) as u64
}

/// One bound, compiled `[models.logic]` part: pin tables, register state,
/// compiled expressions. Pure logic; it computes LEVELS; the caller owns
/// voltage thresholds, net wiring, and drivers.
#[derive(Debug)]
pub struct LogicComponent {
    /// The model id / spec name, for diagnostics.
    spec_id: String,
    /// Declared input pin names (index = pin id used everywhere below).
    input_names: Vec<String>,
    /// Declared output names in declaration order.
    output_names: Vec<String>,
    /// Last decided level per input (hysteresis memory for the caller's
    /// threshold decision, and the value expressions read).
    input_levels: Vec<bool>,
    /// The level an UNWIRED instance of each input reads (see module docs).
    input_defaults: Vec<bool>,
    /// Current output levels.
    output_levels: Vec<bool>,
    /// Current output drive enables (false = tri-stated).
    output_enabled: Vec<bool>,
    registers: Vec<CompReg>,
    comb: Vec<CompComb>,
    plan: Vec<PlanStep>,
    tristates: Vec<CompTristate>,
    memories: Vec<CompMemory>,
    /// evalexpr variable names, precomputed: `v_<input>` / `v_<output>`.
    input_vars: Vec<String>,
    output_vars: Vec<String>,
    /// Register bits referenced by any expression: (reg index, bit, var name).
    reg_bit_vars: Vec<(usize, u32, String)>,
    /// Reused evaluation context (keys are stable; values overwritten per tick).
    ctx: HashMapContext<DefaultNumericTypes>,
    /// Cycle warnings raised at validation (surfaced by lint / bind).
    pub warnings: Vec<String>,
}

/// Lower a validated AST into fully parenthesized evalexpr source over
/// sanitized boolean variables.
fn lower_expr(e: &LogicExpr, out: &mut String) {
    match e {
        LogicExpr::Const(true) => out.push_str("true"),
        LogicExpr::Const(false) => out.push_str("false"),
        LogicExpr::Name(n) => {
            out.push_str("v_");
            out.push_str(n);
        }
        LogicExpr::Bit(n, i) => {
            out.push_str(&format!("b_{n}_{i}"));
        }
        LogicExpr::Not(a) => {
            out.push_str("(!");
            lower_expr(a, out);
            out.push(')');
        }
        LogicExpr::And(a, b) => {
            out.push('(');
            lower_expr(a, out);
            out.push_str(" && ");
            lower_expr(b, out);
            out.push(')');
        }
        LogicExpr::Or(a, b) => {
            out.push('(');
            lower_expr(a, out);
            out.push_str(" || ");
            lower_expr(b, out);
            out.push(')');
        }
        // Boolean XOR: evalexpr has no `^`, but `!=` on booleans is exactly it.
        LogicExpr::Xor(a, b) => {
            out.push('(');
            lower_expr(a, out);
            out.push_str(" != ");
            lower_expr(b, out);
            out.push(')');
        }
    }
}

/// Tarjan strongly-connected components over the comb dependency graph
/// (`deps[i]` = output indices output `i` references). Returns SCCs in
/// TOPOLOGICAL order (dependencies before dependents).
fn tarjan_sccs(deps: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = deps.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    // Iterative Tarjan (explicit frame stack: node, child cursor).
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        let mut frames: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&mut (v, ref mut ci)) = frames.last_mut() {
            if *ci == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if *ci < deps[v].len() {
                let w = deps[v][*ci];
                *ci += 1;
                if index[w] == usize::MAX {
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                frames.pop();
                if let Some(&mut (p, _)) = frames.last_mut() {
                    low[p] = low[p].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut scc = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        scc.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(scc);
                }
            }
        }
    }
    // Tarjan emits SCCs in reverse topological order.
    sccs.reverse();
    sccs
}

impl LogicComponent {
    /// Compile a validated `[models.logic]` block. Runs `Logic::validate`,
    /// lowers and compiles every expression, builds the evaluation plan, and
    /// exhaustively verifies fixpoint convergence for every combinational
    /// cycle. All failures are named.
    pub fn compile(spec_id: &str, logic: &Logic) -> Result<Self, LogicCompileError> {
        let validated = logic.validate_with_features(&[Logic::FEATURE_MEMORY])?;

        let input_names: Vec<String> = logic.inputs.clone();
        let output_names: Vec<String> = logic.outputs.clone();
        let input_idx: HashMap<&str, usize> = input_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let output_idx: HashMap<&str, usize> = output_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let reg_idx: HashMap<&str, usize> = logic
            .registers
            .iter()
            .enumerate()
            .map(|(i, r)| (r.name.as_str(), i))
            .collect();

        // ── Addressable memories ──
        let mut memories: Vec<CompMemory> = Vec::with_capacity(validated.memories.len());
        for m in &validated.memories {
            let mask = if m.bits == 64 {
                u64::MAX
            } else {
                (1u64 << m.bits) - 1
            };
            memories.push(CompMemory {
                name: m.name.clone(),
                mask,
                address: m.address.iter().map(|p| input_idx[p.as_str()]).collect(),
                writes: m
                    .write
                    .iter()
                    .map(|w| CompMemoryWrite {
                        pin: input_idx[w.pin.as_str()],
                        edge: w.edge,
                        prev: false,
                        gates: m
                            .write_gates
                            .iter()
                            .map(|g| (input_idx[g.pin.as_str()], g.active))
                            .collect(),
                    })
                    .chain(m.write_cycles.iter().map(|w| {
                        CompMemoryWrite {
                            pin: input_idx[w.pin.as_str()],
                            edge: w.edge,
                            prev: false,
                            gates: w
                                .gates
                                .iter()
                                .map(|g| (input_idx[g.pin.as_str()], g.active))
                                .collect(),
                        }
                    }))
                    .collect(),
                read_gates: m
                    .read_gates
                    .iter()
                    .map(|g| (input_idx[g.pin.as_str()], g.active))
                    .collect(),
                data_in_names: m.data_in.clone(),
                data_in_levels: vec![false; m.data_in.len()],
                data_out: m.data_out.iter().map(|p| output_idx[p.as_str()]).collect(),
                byte_load_timeout_s: m.byte_load_timeout_s,
                program_time_s: m.program_time_s,
                storage: Arc::new(Mutex::new(ParallelMemoryStorage {
                    cells: vec![m.init & mask; m.words as usize].into_boxed_slice(),
                    protection: m.software_data_protection.as_ref().map(|p| {
                        SoftwareProtectionState {
                            enabled: p.initial,
                            enable: p
                                .enable
                                .iter()
                                .map(|w| (w.address as usize, w.value))
                                .collect(),
                            disable: p
                                .disable
                                .iter()
                                .map(|w| (w.address as usize, w.value))
                                .collect(),
                            page_words: m.page_words.unwrap_or(1) as usize,
                            command_buffer: Vec::new(),
                            write_window: None,
                        }
                    }),
                    page_words: m.page_words.unwrap_or(1) as usize,
                    timed_page: None,
                    busy: None,
                    last_timed_write_cycle: None,
                })),
            });
        }

        // ── Registers ──
        let registers: Vec<CompReg> = logic
            .registers
            .iter()
            .map(|r| {
                let mask = if r.bits == 64 {
                    u64::MAX
                } else {
                    (1u64 << r.bits) - 1
                };
                CompReg {
                    name: r.name.clone(),
                    bits: r.bits,
                    mask,
                    state: r.init & mask,
                    clock: r
                        .clock
                        .as_ref()
                        .map(|c| (input_idx[c.pin.as_str()], c.edge)),
                    prev_clock: false,
                    resets: r
                        .resets
                        .iter()
                        .map(|rst| (input_idx[rst.pin.as_str()], rst.active, rst.value))
                        .collect(),
                    op: r.op,
                    data_in: r.data_in.as_ref().map(|d| {
                        if let Some(&ri) = reg_idx.get(d.as_str()) {
                            DataSrc::Reg(ri)
                        } else {
                            DataSrc::Pin(input_idx[d.as_str()])
                        }
                    }),
                    load: r.load.as_ref().map(|l| {
                        (
                            input_idx[l.pin.as_str()],
                            l.active,
                            l.data.iter().map(|p| input_idx[p.as_str()]).collect(),
                        )
                    }),
                    clock_enable: r
                        .clock_enable
                        .as_ref()
                        .map(|e| (input_idx[e.pin.as_str()], e.active)),
                }
            })
            .collect();

        // ── Comb compilation ──
        let mut comb: Vec<CompComb> = Vec::with_capacity(validated.comb.len());
        // Plan nodes index `comb`, not the full output vector: memory-backed
        // outputs can be interleaved with expression-backed outputs.
        let comb_idx: HashMap<&str, usize> = validated
            .comb
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.as_str(), i))
            .collect();
        let mut deps: Vec<Vec<usize>> = vec![Vec::new(); validated.comb.len()];
        let mut reg_bit_vars: Vec<(usize, u32, String)> = Vec::new();
        let mut seen_bits: std::collections::HashSet<(usize, u32)> =
            std::collections::HashSet::new();
        for (comb_i, (out_name, ast)) in validated.comb.iter().enumerate() {
            let out = output_idx[out_name.as_str()];
            let mut src = String::new();
            lower_expr(ast, &mut src);
            let node = build_operator_tree::<DefaultNumericTypes>(&src).map_err(|e| {
                LogicCompileError::Lowering {
                    output: out_name.clone(),
                    message: e.to_string(),
                }
            })?;
            let mut names = Vec::new();
            let mut bits = Vec::new();
            ast.collect_refs(&mut names, &mut bits);
            for n in names {
                if let Some(&dependency) = comb_idx.get(n) {
                    deps[comb_i].push(dependency);
                }
            }
            for (n, i) in bits {
                let ri = reg_idx[n];
                if seen_bits.insert((ri, i)) {
                    reg_bit_vars.push((ri, i, format!("b_{n}_{i}")));
                }
            }
            comb.push(CompComb { out, node });
        }

        // ── Evaluation plan: SCCs in topological order ──
        let plan: Vec<PlanStep> = tarjan_sccs(&deps)
            .into_iter()
            .map(|mut scc| {
                if scc.len() == 1 && !deps[scc[0]].contains(&scc[0]) {
                    PlanStep::Single(scc[0])
                } else {
                    // Declaration order within the SCC = resolution order.
                    scc.sort_unstable();
                    PlanStep::Scc(scc)
                }
            })
            .collect();

        // ── Tri-state groups ──
        let mut tristates: Vec<CompTristate> = Vec::new();
        for (group, ts) in &logic.tristate {
            let outs = logic.expand_tristate_group(group)?;
            tristates.push(CompTristate {
                outputs: outs.iter().map(|o| output_idx[o.as_str()]).collect(),
                enable: input_idx[ts.enable.as_str()],
                active: ts.active,
            });
        }

        // ── Unwired-pin defaults (see module docs) ──
        let mut input_defaults = vec![false; input_names.len()];
        for r in &registers {
            for &(pin, active, _) in &r.resets {
                // Released = inactive.
                input_defaults[pin] = match active {
                    Level::Low => true,
                    Level::High => false,
                };
            }
            if let Some((pin, active, _)) = &r.load {
                input_defaults[*pin] = match active {
                    Level::Low => true,
                    Level::High => false,
                };
            }
            if let Some((pin, active)) = r.clock_enable {
                // Clock enabled = active.
                input_defaults[pin] = match active {
                    Level::Low => false,
                    Level::High => true,
                };
            }
        }
        for ts in &tristates {
            // Outputs driven = active.
            input_defaults[ts.enable] = match ts.active {
                Level::Low => false,
                Level::High => true,
            };
        }
        for m in &memories {
            // Unwired memory controls fail safe: selects/enables are inactive,
            // and the write strobe starts released so the first observed idle
            // level cannot manufacture a write edge.
            for &(pin, active) in &m.read_gates {
                input_defaults[pin] = match active {
                    Level::Low => true,
                    Level::High => false,
                };
            }
            for write in &m.writes {
                for &(pin, active) in &write.gates {
                    input_defaults[pin] = match active {
                        Level::Low => true,
                        Level::High => false,
                    };
                }
                input_defaults[write.pin] = match write.edge {
                    Edge::Rising => true,
                    Edge::Falling => false,
                };
            }
        }

        for m in &mut memories {
            for write in &mut m.writes {
                write.prev = input_defaults[write.pin];
            }
        }

        // ── Initial output levels (power-on) ──
        let mut output_levels = vec![false; output_names.len()];
        for (name, &v) in &logic.init {
            output_levels[output_idx[name.as_str()]] = v != 0;
        }

        let input_vars: Vec<String> = input_names.iter().map(|n| format!("v_{n}")).collect();
        let output_vars: Vec<String> = output_names.iter().map(|n| format!("v_{n}")).collect();

        let mut lc = LogicComponent {
            spec_id: spec_id.to_string(),
            input_levels: input_defaults.clone(),
            input_defaults,
            output_enabled: vec![true; output_names.len()],
            input_names,
            output_names,
            output_levels,
            registers,
            comb,
            plan,
            tristates,
            memories,
            input_vars,
            output_vars,
            reg_bit_vars,
            ctx: HashMapContext::new(),
            warnings: validated.warnings,
        };
        lc.check_convergence()?;
        Ok(lc)
    }

    /// Exhaustively verify that every combinational cycle settles within
    /// [`COMB_FIXPOINT_BOUND`] Gauss–Seidel sweeps for EVERY assignment of
    /// its external inputs and EVERY seed state of its members. Refuses
    /// (named error) when it cannot verify.
    fn check_convergence(&mut self) -> Result<(), LogicCompileError> {
        // Collect the plan's SCC steps first (borrow gymnastics: we mutate ctx
        // during evaluation).
        let scc_steps: Vec<Vec<usize>> = self
            .plan
            .iter()
            .filter_map(|s| match s {
                PlanStep::Scc(m) => Some(m.clone()),
                PlanStep::Single(_) => None,
            })
            .collect();

        for members in scc_steps {
            // External variables: everything the member expressions read that
            // is not itself a member. Enumerating non-member OUTPUTS as free
            // bits over-approximates reachable states, safe: convergence
            // for a superset of states implies it for the reachable set.
            let member_outputs: std::collections::HashSet<usize> = members
                .iter()
                .map(|&comb_i| self.comb[comb_i].out)
                .collect();
            let mut externals: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for &mi in &members {
                let node = &self.comb[mi].node;
                for var in node.iter_variable_identifiers() {
                    let is_member = self
                        .output_vars
                        .iter()
                        .enumerate()
                        .any(|(oi, ov)| ov == var && member_outputs.contains(&oi));
                    if !is_member && seen.insert(var.to_string()) {
                        externals.push(var.to_string());
                    }
                }
            }
            let m = externals.len() as u32;
            let k = members.len() as u32;
            let names: Vec<String> = members
                .iter()
                .map(|&comb_i| self.output_names[self.comb[comb_i].out].clone())
                .collect();
            if m + k > CONVERGENCE_ENUM_CAP {
                return Err(LogicCompileError::TooWideToVerify {
                    outputs: names,
                    external: m,
                    members: k,
                    total: m + k,
                    cap: CONVERGENCE_ENUM_CAP,
                });
            }

            for ext_bits in 0u64..(1u64 << m) {
                for (i, var) in externals.iter().enumerate() {
                    let v = (ext_bits >> i) & 1 == 1;
                    let _ = self.ctx.set_value(var.clone(), Value::Boolean(v));
                }
                for seed_bits in 0u64..(1u64 << k) {
                    let mut levels: Vec<bool> =
                        (0..k as usize).map(|i| (seed_bits >> i) & 1 == 1).collect();
                    for (i, &comb_i) in members.iter().enumerate() {
                        let out = self.comb[comb_i].out;
                        let _ = self
                            .ctx
                            .set_value(self.output_vars[out].clone(), Value::Boolean(levels[i]));
                    }
                    let mut settled = false;
                    for _ in 0..COMB_FIXPOINT_BOUND {
                        let mut changed = false;
                        for (i, &comb_i) in members.iter().enumerate() {
                            let out = self.comb[comb_i].out;
                            let v = self.comb[comb_i]
                                .node
                                .eval_boolean_with_context(&self.ctx)
                                .unwrap_or(false);
                            if v != levels[i] {
                                levels[i] = v;
                                changed = true;
                                let _ = self
                                    .ctx
                                    .set_value(self.output_vars[out].clone(), Value::Boolean(v));
                            }
                        }
                        if !changed {
                            settled = true;
                            break;
                        }
                    }
                    if !settled {
                        let ext_desc: Vec<String> = externals
                            .iter()
                            .enumerate()
                            .map(|(i, v)| format!("{v}={}", (ext_bits >> i) & 1))
                            .collect();
                        return Err(LogicCompileError::NonConvergent {
                            outputs: names,
                            bound: COMB_FIXPOINT_BOUND,
                            witness: format!(
                                "externals [{}], seed {:#b}",
                                ext_desc.join(", "),
                                seed_bits
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// The model id / spec name this component was compiled from.
    pub fn spec_id(&self) -> &str {
        &self.spec_id
    }

    /// Declared input pin names.
    pub fn input_names(&self) -> &[String] {
        &self.input_names
    }

    /// Declared output names, in declaration order.
    pub fn output_names(&self) -> &[String] {
        &self.output_names
    }

    /// Does the spec declare this input pin?
    pub fn has_input(&self, name: &str) -> bool {
        self.input_names.iter().any(|n| n == name)
    }

    /// Does the spec declare this output?
    pub fn has_output(&self, name: &str) -> bool {
        self.output_names.iter().any(|n| n == name)
    }

    /// True when the part has at least one clocked register; the parts whose
    /// sub-chunk pulse trains demand edge-granular replay.
    pub fn is_sequential(&self) -> bool {
        self.registers.iter().any(|r| r.clock.is_some())
            || self.memories.iter().any(|m| !m.writes.is_empty())
    }

    /// Input pins that participate in sequential behaviour (register clocks,
    /// resets, loads + load data, enables, serial data): the pins whose edge
    /// timing matters. Used by the scheduler to decide edge-replay membership.
    pub fn sequential_pins(&self) -> Vec<&str> {
        let mut pins: Vec<&str> = Vec::new();
        for r in &self.registers {
            if let Some((p, _)) = r.clock {
                pins.push(self.input_names[p].as_str());
            }
            for &(p, _, _) in &r.resets {
                pins.push(self.input_names[p].as_str());
            }
            if let Some((p, _, data)) = &r.load {
                pins.push(self.input_names[*p].as_str());
                pins.extend(data.iter().map(|&p| self.input_names[p].as_str()));
            }
            if let Some((p, _)) = r.clock_enable {
                pins.push(self.input_names[p].as_str());
            }
            if let Some(DataSrc::Pin(p)) = r.data_in {
                pins.push(self.input_names[p].as_str());
            }
        }
        for m in &self.memories {
            pins.extend(m.address.iter().map(|&p| self.input_names[p].as_str()));
            for write in &m.writes {
                pins.push(self.input_names[write.pin].as_str());
                pins.extend(
                    write
                        .gates
                        .iter()
                        .map(|&(p, _)| self.input_names[p].as_str()),
                );
            }
            pins.extend(
                m.read_gates
                    .iter()
                    .map(|&(p, _)| self.input_names[p].as_str()),
            );
            pins.extend(m.data_in_names.iter().map(String::as_str));
        }
        pins.sort_unstable();
        pins.dedup();
        pins
    }

    /// Current level of an input pin (the last decided sample).
    pub fn input_level(&self, name: &str) -> Option<bool> {
        let i = self.input_names.iter().position(|n| n == name)?;
        Some(self.input_levels[i])
    }

    /// Current logic level of an output.
    pub fn output_level(&self, name: &str) -> Option<bool> {
        let i = self.output_names.iter().position(|n| n == name)?;
        Some(self.output_levels[i])
    }

    /// Is an output currently driving (not tri-stated)?
    pub fn output_enabled(&self, name: &str) -> Option<bool> {
        let i = self.output_names.iter().position(|n| n == name)?;
        Some(self.output_enabled[i])
    }

    /// Addressable memories declared by this component, sharing their backing
    /// cells with the tick evaluator. Empty for ordinary logic parts.
    pub(crate) fn memory_ports(&self) -> Vec<ParallelMemoryPort> {
        self.memories
            .iter()
            .map(|m| ParallelMemoryPort {
                name: m.name.clone(),
                address: m
                    .address
                    .iter()
                    .map(|&p| self.input_names[p].clone())
                    .collect(),
                writes: m
                    .writes
                    .iter()
                    .map(|w| ParallelMemoryWritePort {
                        pin: self.input_names[w.pin].clone(),
                        edge: w.edge,
                        gates: w
                            .gates
                            .iter()
                            .map(|&(p, active)| (self.input_names[p].clone(), active))
                            .collect(),
                    })
                    .collect(),
                read_gates: m
                    .read_gates
                    .iter()
                    .map(|&(p, active)| (self.input_names[p].clone(), active))
                    .collect(),
                data_in: m.data_in_names.clone(),
                data_out: m
                    .data_out
                    .iter()
                    .map(|&p| self.output_names[p].clone())
                    .collect(),
                byte_load_timeout_s: m.byte_load_timeout_s,
                program_time_s: m.program_time_s,
                storage: m.storage.clone(),
                mask: m.mask,
            })
            .collect()
    }

    /// All outputs: `(name, level, enabled)`.
    pub fn outputs(&self) -> impl Iterator<Item = (&str, bool, bool)> {
        self.output_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), self.output_levels[i], self.output_enabled[i]))
    }

    /// Current value of a register.
    pub fn register(&self, name: &str) -> Option<u64> {
        self.registers
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.state)
    }

    /// All registers: `(name, value)`.
    pub fn registers(&self) -> impl Iterator<Item = (&str, u64)> {
        self.registers.iter().map(|r| (r.name.as_str(), r.state))
    }

    /// Declared width of a register, if the spec has one by this name.
    pub fn register_bits(&self, name: &str) -> Option<u32> {
        self.registers
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.bits)
    }

    /// Overwrite a register's value (the chain-controller mirror / test-latch
    /// path). Returns false when the spec has no such register. Callers that
    /// need the outputs to reflect the new state must call
    /// [`LogicComponent::refresh_outputs`] afterwards.
    pub fn set_register(&mut self, name: &str, value: u64) -> bool {
        match self.registers.iter_mut().find(|r| r.name == name) {
            Some(r) => {
                r.state = value & r.mask;
                true
            }
            None => false,
        }
    }

    /// One evaluation step. `sample(name, prev)` returns the decided level of
    /// a WIRED input pin given its previous level (the caller applies its
    /// voltage thresholds + hysteresis), or `None` for an unwired pin (which
    /// then reads its default, see module docs).
    pub fn tick(&mut self, sample: &mut dyn FnMut(&str, bool) -> Option<bool>) {
        // 1. Sample every input.
        for i in 0..self.input_names.len() {
            let prev = self.input_levels[i];
            self.input_levels[i] = match sample(&self.input_names[i], prev) {
                Some(level) => level,
                None => self.input_defaults[i],
            };
        }
        // Bidirectional memory data pins may be declared outputs, so they do
        // not occur in `input_names`. Sample them explicitly while the memory
        // is tri-stated; the caller resolves the same physical role/net.
        for m in &mut self.memories {
            for i in 0..m.data_in_names.len() {
                let prev = m.data_in_levels[i];
                m.data_in_levels[i] = sample(&m.data_in_names[i], prev).unwrap_or(false);
            }
        }

        // 2. Registers: next values from the PRE-tick state, then commit
        //    simultaneously (tied-clock silicon semantics).
        let old_state: Vec<u64> = self.registers.iter().map(|r| r.state).collect();
        for ri in 0..self.registers.len() {
            let (cur_clock, next) = {
                let r = &self.registers[ri];
                let cur_clock = r.clock.map(|(p, _)| self.input_levels[p]).unwrap_or(false);
                let edge_fired = match r.clock {
                    Some((_, Edge::Rising)) => cur_clock && !r.prev_clock,
                    Some((_, Edge::Falling)) => !cur_clock && r.prev_clock,
                    None => false,
                };
                let enabled = match r.clock_enable {
                    Some((p, active)) => active.is_active(self.input_levels[p]),
                    None => true,
                };
                // Priority: reset > load > clock edge > hold.
                let mut next = r.state;
                let active_reset = r
                    .resets
                    .iter()
                    .find(|&&(p, active, _)| active.is_active(self.input_levels[p]));
                if let Some(&(_, _, value)) = active_reset {
                    next = value & r.mask;
                } else if let Some((p, active, data)) = &r.load {
                    if active.is_active(self.input_levels[*p]) {
                        let mut v = 0u64;
                        for (bit, &pin) in data.iter().enumerate() {
                            if self.input_levels[pin] {
                                v |= 1u64 << bit;
                            }
                        }
                        next = v;
                    } else if edge_fired && enabled {
                        next = self.apply_op(r, &old_state);
                    }
                } else if edge_fired && enabled {
                    next = self.apply_op(r, &old_state);
                }
                (cur_clock, next)
            };
            let r = &mut self.registers[ri];
            r.prev_clock = cur_clock;
            r.state = next & r.mask;
        }

        // 2b. Parallel memories commit on their declared, fully-qualified
        // write edge. Address and data are sampled from this exact tick.
        for m in &mut self.memories {
            let mut qualified = false;
            for write in &mut m.writes {
                let cur = self.input_levels[write.pin];
                let fired = match write.edge {
                    Edge::Rising => cur && !write.prev,
                    Edge::Falling => !cur && write.prev,
                };
                write.prev = cur;
                let enabled = write
                    .gates
                    .iter()
                    .all(|&(pin, active)| active.is_active(self.input_levels[pin]));
                qualified |= fired && enabled;
            }
            // If two equivalent cycles qualify on one solver tick, this is
            // still one physical bus write, not two byte loads.
            if !qualified {
                continue;
            }
            let address = m.address.iter().enumerate().fold(0usize, |a, (bit, &pin)| {
                a | (usize::from(self.input_levels[pin]) << bit)
            });
            let value = m
                .data_in_levels
                .iter()
                .enumerate()
                .fold(0u64, |v, (bit, &high)| v | (u64::from(high) << bit))
                & m.mask;
            let _ = m
                .storage
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write(address, value, m.mask, false);
        }

        // 3+4. Comb evaluation + tri-state.
        self.refresh_outputs();
    }

    /// Apply a register's clocked op to the PRE-tick state snapshot.
    fn apply_op(&self, r: &CompReg, old_state: &[u64]) -> u64 {
        let din_bit = |src: &DataSrc| -> u64 {
            match src {
                DataSrc::Pin(p) => u64::from(self.input_levels[*p]),
                // Validation restricts shift data to pins; unreachable, kept total.
                DataSrc::Reg(ri) => old_state[*ri] & 1,
            }
        };
        match r.op {
            Some(RegisterOp::ShiftLeft) => {
                let d = r.data_in.as_ref().map(din_bit).unwrap_or(0);
                (r.state.wrapping_shl(1) | d) & r.mask
            }
            Some(RegisterOp::ShiftRight) => {
                let d = r.data_in.as_ref().map(din_bit).unwrap_or(0);
                (r.state >> 1) | (d.wrapping_shl(r.bits - 1))
            }
            Some(RegisterOp::Load) => match r.data_in {
                Some(DataSrc::Reg(ri)) => old_state[ri] & r.mask,
                Some(DataSrc::Pin(p)) => u64::from(self.input_levels[p]),
                None => r.state,
            },
            Some(RegisterOp::CountUp) => r.state.wrapping_add(1) & r.mask,
            Some(RegisterOp::CountDown) => r.state.wrapping_sub(1) & r.mask,
            None => r.state,
        }
    }

    /// Re-evaluate the combinational outputs and tri-state enables from the
    /// CURRENT input levels and register state (also the entry point after an
    /// external register overwrite via [`LogicComponent::set_register`]).
    pub fn refresh_outputs(&mut self) {
        // Bind every variable the expressions read.
        for i in 0..self.input_names.len() {
            let _ = self.ctx.set_value(
                self.input_vars[i].clone(),
                Value::Boolean(self.input_levels[i]),
            );
        }

        // Memory reads are combinational with address. Publish them before
        // binding output variables so ordinary expressions can consume a
        // memory-backed output in the same refresh. Do not touch storage while
        // the bus is tri-stated: besides avoiding meaningless work, this lets
        // a command protocol retain a candidate prefix across several writes;
        // an actual enabled read is what terminates a lone, incomplete prefix.
        for m in &self.memories {
            let read_enabled = m
                .read_gates
                .iter()
                .all(|&(pin, active)| active.is_active(self.input_levels[pin]));
            if !read_enabled {
                continue;
            }
            let address = m.address.iter().enumerate().fold(0usize, |a, (bit, &pin)| {
                a | (usize::from(self.input_levels[pin]) << bit)
            });
            let mut storage = m.storage.lock().unwrap_or_else(|e| e.into_inner());
            let word = storage.read(address, m.mask).unwrap_or(0);
            for (bit, &out) in m.data_out.iter().enumerate() {
                self.output_levels[out] = word & (1u64 << bit) != 0;
            }
        }
        for i in 0..self.output_names.len() {
            let _ = self.ctx.set_value(
                self.output_vars[i].clone(),
                Value::Boolean(self.output_levels[i]),
            );
        }
        for (ri, bit, var) in &self.reg_bit_vars {
            let v = (self.registers[*ri].state >> bit) & 1 == 1;
            let _ = self.ctx.set_value(var.clone(), Value::Boolean(v));
        }

        for step in &self.plan {
            match step {
                PlanStep::Single(comb_i) => {
                    let out = self.comb[*comb_i].out;
                    let v = self.comb[*comb_i]
                        .node
                        .eval_boolean_with_context(&self.ctx)
                        .unwrap_or(false);
                    self.output_levels[out] = v;
                    let _ = self
                        .ctx
                        .set_value(self.output_vars[out].clone(), Value::Boolean(v));
                }
                PlanStep::Scc(members) => {
                    // Gauss–Seidel sweeps in declaration order, seeded from
                    // the previous stable levels. Convergence within the
                    // bound was verified exhaustively at compile time; the
                    // check below is defense in depth only.
                    let mut settled = false;
                    for _ in 0..COMB_FIXPOINT_BOUND {
                        let mut changed = false;
                        for &comb_i in members {
                            let out = self.comb[comb_i].out;
                            let v = self.comb[comb_i]
                                .node
                                .eval_boolean_with_context(&self.ctx)
                                .unwrap_or(false);
                            if v != self.output_levels[out] {
                                self.output_levels[out] = v;
                                changed = true;
                                let _ = self
                                    .ctx
                                    .set_value(self.output_vars[out].clone(), Value::Boolean(v));
                            }
                        }
                        if !changed {
                            settled = true;
                            break;
                        }
                    }
                    if !settled {
                        // Unreachable for a compiled spec (see check_convergence);
                        // scream rather than silently hold a wrong level.
                        eprintln!(
                            "ERROR: logic '{}': comb cycle failed to settle within {} sweeps \
                             at runtime; this should have been refused at compile time",
                            self.spec_id, COMB_FIXPOINT_BOUND
                        );
                    }
                }
            }
        }

        // Tri-state enables.
        for i in 0..self.output_enabled.len() {
            self.output_enabled[i] = true;
        }
        for ts in &self.tristates {
            let en = ts.active.is_active(self.input_levels[ts.enable]);
            if !en {
                for &oi in &ts.outputs {
                    self.output_enabled[oi] = false;
                }
            }
        }
        for m in &self.memories {
            let enabled = m
                .read_gates
                .iter()
                .all(|&(pin, active)| active.is_active(self.input_levels[pin]));
            if !enabled {
                for &oi in &m.data_out {
                    self.output_enabled[oi] = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(toml_src: &str) -> LogicComponent {
        let logic: Logic = toml::from_str(toml_src).expect("logic TOML parses");
        LogicComponent::compile("test", &logic).expect("compiles")
    }

    /// Drive a tick from a plain name->level map (unlisted pins = unwired).
    fn tick_with(lc: &mut LogicComponent, levels: &[(&str, bool)]) {
        let map: HashMap<&str, bool> = levels.iter().copied().collect();
        lc.tick(&mut |name, _prev| map.get(name).copied());
    }

    const HC595: &str = r#"
inputs  = ["ser", "srclk", "rclk", "srclr_n", "oe_n"]
outputs = ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh", "qh_serial"]

[[register]]
name = "shift"
bits = 8
clock = { pin = "srclk", edge = "rising" }
reset = { pin = "srclr_n", active = "low", value = 0 }
op = "shift_left"
data_in = "ser"

[[register]]
name = "store"
bits = 8
clock = { pin = "rclk", edge = "rising" }
op = "load"
data_in = "shift"

[comb]
"qa" = "store[0]"
"qb" = "store[1]"
"qc" = "store[2]"
"qd" = "store[3]"
"qe" = "store[4]"
"qf" = "store[5]"
"qg" = "store[6]"
"qh" = "store[7]"
"qh_serial" = "shift[7]"

[tristate]
"qa..qh" = { enable = "oe_n", active = "low" }
"#;

    /// Small parallel EEPROM with the same bus/control semantics as a 28C256.
    /// Four words keep the evaluator test cheap while exercising address,
    /// bidirectional data, qualified WE edges, and exact read gating.
    const PARALLEL_EEPROM: &str = r#"
inputs  = ["a0", "a1", "ce_n", "oe_n", "we_n"]
outputs = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]

[[memory]]
name = "cell"
words = 4
bits = 8
page_words = 4
init = 0xff
address = ["a0", "a1"]
write = { pin = "we_n", edge = "rising" }
write_gates = [{ pin = "ce_n", active = "low" }]
read_gates = [
  { pin = "ce_n", active = "low" },
  { pin = "oe_n", active = "low" },
  { pin = "we_n", active = "high" },
]
data_in = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]
data_out = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]
[memory.software_data_protection]
initial = false
enable = [
    { address = 3, value = 0xAA },
    { address = 2, value = 0x55 },
    { address = 3, value = 0xA0 },
]
disable = [
    { address = 3, value = 0xAA },
    { address = 2, value = 0x55 },
    { address = 3, value = 0x80 },
    { address = 3, value = 0xAA },
    { address = 2, value = 0x55 },
    { address = 3, value = 0x20 },
]
"#;

    /// shiftOut(MSBFIRST) of `byte`: per bit, set SER then pulse SRCLK.
    fn shift_out_msb(lc: &mut LogicComponent, byte: u8, srclr: bool, oe: bool) {
        for bit in (0..8).rev() {
            let b = (byte >> bit) & 1 == 1;
            tick_with(
                lc,
                &[
                    ("ser", b),
                    ("srclk", false),
                    ("rclk", false),
                    ("srclr_n", srclr),
                    ("oe_n", oe),
                ],
            );
            tick_with(
                lc,
                &[
                    ("ser", b),
                    ("srclk", true),
                    ("rclk", false),
                    ("srclr_n", srclr),
                    ("oe_n", oe),
                ],
            );
            tick_with(
                lc,
                &[
                    ("ser", b),
                    ("srclk", false),
                    ("rclk", false),
                    ("srclr_n", srclr),
                    ("oe_n", oe),
                ],
            );
        }
    }

    #[test]
    fn hc595_shifts_and_latches_msb_first() {
        let mut lc = compile(HC595);
        shift_out_msb(&mut lc, 0xA6, true, false);
        assert_eq!(
            lc.register("shift"),
            Some(0xA6),
            "shift register after 8 clocks"
        );
        assert_eq!(
            lc.register("store"),
            Some(0x00),
            "store unlatched until RCLK"
        );
        // qh_serial tracks the shift register's top bit before the latch.
        assert_eq!(
            lc.output_level("qh_serial"),
            Some(true),
            "0xA6 bit7 at the tap"
        );
        // RCLK pulse latches.
        tick_with(
            &mut lc,
            &[
                ("srclk", false),
                ("rclk", true),
                ("srclr_n", true),
                ("oe_n", false),
            ],
        );
        assert_eq!(
            lc.register("store"),
            Some(0xA6),
            "RCLK rising latched shift->store"
        );
        // qa..qh mirror store bits 0..7.
        let byte = 0xA6u8;
        for (i, q) in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"]
            .iter()
            .enumerate()
        {
            assert_eq!(
                lc.output_level(q),
                Some((byte >> i) & 1 == 1),
                "{q} = store[{i}]"
            );
        }
    }

    #[test]
    fn hc595_srclr_is_dominant_over_clock() {
        let mut lc = compile(HC595);
        shift_out_msb(&mut lc, 0xFF, true, false);
        assert_eq!(lc.register("shift"), Some(0xFF));
        // Assert clear, then clock while held: the register stays cleared
        // (TI SN74HC595 function table: SRCLR L + SRCLK X -> cleared).
        tick_with(
            &mut lc,
            &[("srclk", false), ("srclr_n", false), ("oe_n", false)],
        );
        assert_eq!(
            lc.register("shift"),
            Some(0x00),
            "clear wipes the shift register"
        );
        tick_with(
            &mut lc,
            &[
                ("ser", true),
                ("srclk", true),
                ("srclr_n", false),
                ("oe_n", false),
            ],
        );
        assert_eq!(
            lc.register("shift"),
            Some(0x00),
            "a clock edge while SRCLR is held low does not shift"
        );
    }

    #[test]
    fn hc595_tied_clocks_latch_one_step_behind() {
        // SRCLK and RCLK rising on the SAME tick: store captures the
        // PRE-shift value (TI datasheet: with tied clocks the shift register
        // is one clock pulse ahead of the storage register).
        let mut lc = compile(HC595);
        shift_out_msb(&mut lc, 0x01, true, false);
        assert_eq!(lc.register("shift"), Some(0x01));
        tick_with(
            &mut lc,
            &[
                ("ser", true),
                ("srclk", true),
                ("rclk", true),
                ("srclr_n", true),
                ("oe_n", false),
            ],
        );
        assert_eq!(lc.register("shift"), Some(0x03), "shift took the new bit");
        assert_eq!(
            lc.register("store"),
            Some(0x01),
            "store captured the pre-shift value"
        );
    }

    #[test]
    fn hc595_oe_high_tristates_parallel_outputs_only() {
        let mut lc = compile(HC595);
        tick_with(&mut lc, &[("oe_n", true), ("srclr_n", true)]);
        for q in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"] {
            assert_eq!(
                lc.output_enabled(q),
                Some(false),
                "{q} tri-stated while OE_n high"
            );
        }
        assert_eq!(
            lc.output_enabled("qh_serial"),
            Some(true),
            "the serial tap is not OE-gated on a 74HC595"
        );
        tick_with(&mut lc, &[("oe_n", false), ("srclr_n", true)]);
        assert_eq!(lc.output_enabled("qa"), Some(true), "OE_n low re-enables");
    }

    /// The silicon-correct 74HC165 (shift toward QH: QH shows H, then G, ...).
    const HC165: &str = r#"
inputs  = ["pl_n", "clk", "clk_inh", "ser", "a", "b", "c", "d", "e", "f", "g", "h"]
outputs = ["qh", "qh_n"]

[[register]]
name = "reg"
bits = 8
clock = { pin = "clk", edge = "rising" }
clock_enable = { pin = "clk_inh", active = "low" }
op = "shift_left"
data_in = "ser"
load = { pin = "pl_n", active = "low", data = ["a", "b", "c", "d", "e", "f", "g", "h"] }

[comb]
"qh" = "reg[7]"
"qh_n" = "!reg[7]"
"#;

    #[test]
    fn hc165_loads_and_emits_h_first() {
        let mut lc = compile(HC165);
        // Parallel-load 0b1010_0001 (a=1, f=1, h=1).
        let hi = [("a", true), ("f", true), ("h", true)];
        let mut base: Vec<(&str, bool)> = vec![
            ("pl_n", false),
            ("clk", false),
            ("clk_inh", false),
            ("ser", false),
        ];
        base.extend_from_slice(&hi);
        tick_with(&mut lc, &base);
        assert_eq!(lc.register("reg"), Some(0b1010_0001));
        assert_eq!(
            lc.output_level("qh"),
            Some(true),
            "QH shows H right after load"
        );
        assert_eq!(
            lc.output_level("qh_n"),
            Some(false),
            "QH_n is the complement"
        );

        // Release PL, clock: QH walks H, G, F, ... (silicon direction).
        let expected = [true, false, true, false, false, false, false, true]; // h,g,f,e,d,c,b,a
        assert_eq!(lc.output_level("qh"), Some(expected[0]));
        for want in &expected[1..] {
            tick_with(
                &mut lc,
                &[
                    ("pl_n", true),
                    ("clk", true),
                    ("clk_inh", false),
                    ("ser", false),
                ],
            );
            tick_with(
                &mut lc,
                &[
                    ("pl_n", true),
                    ("clk", false),
                    ("clk_inh", false),
                    ("ser", false),
                ],
            );
            assert_eq!(lc.output_level("qh"), Some(*want));
        }
    }

    #[test]
    fn hc165_clock_inhibit_blocks_shifts() {
        let mut lc = compile(HC165);
        tick_with(
            &mut lc,
            &[
                ("pl_n", false),
                ("clk", false),
                ("clk_inh", false),
                ("h", true),
            ],
        );
        assert_eq!(lc.output_level("qh"), Some(true));
        // CLK_INH high: rising clock does nothing.
        tick_with(&mut lc, &[("pl_n", true), ("clk", true), ("clk_inh", true)]);
        assert_eq!(
            lc.register("reg"),
            Some(0x80),
            "inhibited clock held the register"
        );
    }

    const NOR_LATCH: &str = r#"
inputs  = ["set", "reset"]
outputs = ["q", "qb"]

[comb]
"q"  = "!(set | qb)"
"qb" = "!(reset | q)"

[init]
"q" = 1
"qb" = 0
"#;

    #[test]
    fn nor_latch_matches_spike_recorder_truth_table() {
        let mut lc = compile(NOR_LATCH);
        assert_eq!(lc.warnings.len(), 1, "cycle warning surfaced");
        // Power-on idle: Q HIGH (init).
        assert_eq!(lc.output_level("q"), Some(true), "power-on idle Q HIGH");
        // RESET pulse with no spike: Q stays HIGH.
        tick_with(&mut lc, &[("set", false), ("reset", true)]);
        assert_eq!(lc.output_level("q"), Some(true), "RESET holds idle HIGH");
        // Release: HOLD.
        tick_with(&mut lc, &[("set", false), ("reset", false)]);
        assert_eq!(lc.output_level("q"), Some(true), "hold");
        // Spike (SET pulse): Q LOW.
        tick_with(&mut lc, &[("set", true), ("reset", false)]);
        assert_eq!(lc.output_level("q"), Some(false), "SET drives Q LOW");
        // Spike clears: HELD LOW (the latch memory).
        tick_with(&mut lc, &[("set", false), ("reset", false)]);
        assert_eq!(
            lc.output_level("q"),
            Some(false),
            "held LOW after the pulse"
        );
        // RESET: back to idle HIGH.
        tick_with(&mut lc, &[("set", false), ("reset", true)]);
        assert_eq!(lc.output_level("q"), Some(true), "RESET returns idle HIGH");
    }

    #[test]
    fn non_convergent_cycle_is_refused_at_compile() {
        let logic: Logic = toml::from_str(
            r#"
inputs  = ["x"]
outputs = ["y"]
[comb]
"y" = "!y & !x | x & y & 0 | !y"
"#,
        )
        .unwrap();
        // y = !y (obfuscated enough to have an input): never settles.
        let e = LogicComponent::compile("osc", &logic).unwrap_err();
        assert!(
            matches!(e, LogicCompileError::NonConvergent { .. }),
            "got: {e}"
        );
        let msg = e.to_string();
        assert!(msg.contains("does not converge"), "named error text: {msg}");
    }

    #[test]
    fn too_wide_cycle_is_refused_not_shipped_unverified() {
        // A 13-output ring: converges trivially, but too wide to verify
        // exhaustively under the cap, refused with the named error.
        let outputs: Vec<String> = (0..13).map(|i| format!("y{i}")).collect();
        let mut comb = std::collections::BTreeMap::new();
        for i in 0..13usize {
            comb.insert(format!("y{i}"), format!("y{}", (i + 1) % 13));
        }
        let logic = Logic {
            inputs: vec![],
            outputs,
            comb,
            ..Default::default()
        };
        let e = LogicComponent::compile("ring", &logic).unwrap_err();
        assert!(
            matches!(e, LogicCompileError::TooWideToVerify { members: 13, .. }),
            "got: {e}"
        );
    }

    #[test]
    fn dff_with_preset_and_clear() {
        // The 74HC74 single-flop shape: load-from-pin on rising clock, dual
        // async controls.
        let mut lc = compile(
            r#"
inputs  = ["d", "clk", "pre_n", "clr_n"]
outputs = ["q", "q_n"]
[[register]]
name = "ff"
bits = 1
clock = { pin = "clk", edge = "rising" }
reset = [
  { pin = "clr_n", active = "low", value = 0 },
  { pin = "pre_n", active = "low", value = 1 },
]
op = "load"
data_in = "d"
[comb]
"q" = "ff[0]"
"q_n" = "!ff[0]"
"#,
        );
        // Clock a 1 through D.
        tick_with(
            &mut lc,
            &[
                ("d", true),
                ("clk", false),
                ("pre_n", true),
                ("clr_n", true),
            ],
        );
        tick_with(
            &mut lc,
            &[("d", true), ("clk", true), ("pre_n", true), ("clr_n", true)],
        );
        assert_eq!(
            lc.output_level("q"),
            Some(true),
            "D captured on rising edge"
        );
        assert_eq!(lc.output_level("q_n"), Some(false));
        // D changes while clock low: no effect.
        tick_with(
            &mut lc,
            &[
                ("d", false),
                ("clk", false),
                ("pre_n", true),
                ("clr_n", true),
            ],
        );
        assert_eq!(
            lc.output_level("q"),
            Some(true),
            "level-insensitive between edges"
        );
        // Async clear dominates the clock.
        tick_with(
            &mut lc,
            &[
                ("d", true),
                ("clk", true),
                ("pre_n", true),
                ("clr_n", false),
            ],
        );
        assert_eq!(lc.output_level("q"), Some(false), "CLR_n forces 0");
        // Async preset.
        tick_with(
            &mut lc,
            &[
                ("d", false),
                ("clk", false),
                ("pre_n", false),
                ("clr_n", true),
            ],
        );
        assert_eq!(lc.output_level("q"), Some(true), "PRE_n forces 1");
    }

    #[test]
    fn counter_ops_wrap_at_width() {
        let mut lc = compile(
            r#"
inputs  = ["clk", "rst"]
outputs = ["q0", "q1"]
[[register]]
name = "cnt"
bits = 2
clock = { pin = "clk", edge = "rising" }
reset = { pin = "rst", active = "high", value = 0 }
op = "count_up"
[comb]
"q0" = "cnt[0]"
"q1" = "cnt[1]"
"#,
        );
        for want in [1u64, 2, 3, 0, 1] {
            tick_with(&mut lc, &[("clk", true), ("rst", false)]);
            tick_with(&mut lc, &[("clk", false), ("rst", false)]);
            assert_eq!(lc.register("cnt"), Some(want), "2-bit counter wraps");
        }
    }

    #[test]
    fn unwired_controls_read_released() {
        // Drive only SER/SRCLK; SRCLR_n and OE_n unwired must read released /
        // enabled, so the part shifts and drives normally.
        let mut lc = compile(HC595);
        let seq = [("ser", true), ("srclk", false)];
        tick_with(&mut lc, &seq);
        tick_with(&mut lc, &[("ser", true), ("srclk", true)]);
        assert_eq!(
            lc.register("shift"),
            Some(0x01),
            "unwired SRCLR_n reads released"
        );
        assert_eq!(
            lc.output_enabled("qa"),
            Some(true),
            "unwired OE_n stays enabled"
        );
    }

    #[test]
    fn falling_edge_clock_is_honored() {
        let mut lc = compile(
            r#"
inputs  = ["d", "clkn"]
outputs = ["q"]
[[register]]
name = "ff"
bits = 1
clock = { pin = "clkn", edge = "falling" }
op = "load"
data_in = "d"
[comb]
"q" = "ff[0]"
"#,
        );
        tick_with(&mut lc, &[("d", true), ("clkn", true)]);
        assert_eq!(
            lc.output_level("q"),
            Some(false),
            "rising edge does nothing"
        );
        tick_with(&mut lc, &[("d", true), ("clkn", false)]);
        assert_eq!(lc.output_level("q"), Some(true), "falling edge captures D");
    }

    #[test]
    fn set_register_plus_refresh_drives_outputs() {
        // The chain-mirror / latch_byte path: overwrite store, refresh, read qa..qh.
        let mut lc = compile(HC595);
        assert!(lc.set_register("store", 0x5A));
        assert!(
            !lc.set_register("nonexistent", 1),
            "unknown register refused"
        );
        lc.refresh_outputs();
        for (i, q) in ["qa", "qb", "qc", "qd", "qe", "qf", "qg", "qh"]
            .iter()
            .enumerate()
        {
            assert_eq!(lc.output_level(q), Some((0x5Au8 >> i) & 1 == 1));
        }
    }

    #[test]
    fn gate_expressions_evaluate_truth_tables() {
        // One 74HC00 NAND gate + one XOR (the §1.3 shapes) in a single block.
        let mut lc = compile(
            r#"
inputs  = ["1a", "1b"]
outputs = ["nand_y", "xor_y"]
[comb]
"nand_y" = "!(1a & 1b)"
"xor_y" = "1a ^ 1b"
"#,
        );
        for (a, b, nand, xor) in [
            (false, false, true, false),
            (false, true, true, true),
            (true, false, true, true),
            (true, true, false, false),
        ] {
            tick_with(&mut lc, &[("1a", a), ("1b", b)]);
            assert_eq!(lc.output_level("nand_y"), Some(nand), "NAND({a},{b})");
            assert_eq!(lc.output_level("xor_y"), Some(xor), "XOR({a},{b})");
        }
    }

    #[test]
    fn sequential_pins_cover_clock_and_data() {
        let lc = compile(HC595);
        let pins = lc.sequential_pins();
        for p in ["ser", "srclk", "rclk", "srclr_n"] {
            assert!(pins.contains(&p), "{p} is a sequential pin");
        }
        assert!(!pins.contains(&"oe_n"), "OE is not sequential");
        assert!(lc.is_sequential());
    }

    #[test]
    fn parallel_memory_reads_erased_words_and_honors_all_read_gates() {
        let mut lc = compile(PARALLEL_EEPROM);

        tick_with(
            &mut lc,
            &[
                ("a0", true),
                ("a1", false),
                ("ce_n", false),
                ("oe_n", false),
                ("we_n", true),
            ],
        );
        for bit in 0..8 {
            let io = format!("io{bit}");
            assert_eq!(lc.output_level(&io), Some(true), "erased bit {bit}");
            assert_eq!(lc.output_enabled(&io), Some(true), "read bit {bit} drives");
        }

        // Each gate independently suppresses drive; a memory must never fight
        // the bus master while WE is active.
        for (ce_n, oe_n, we_n) in [
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ] {
            tick_with(&mut lc, &[("ce_n", ce_n), ("oe_n", oe_n), ("we_n", we_n)]);
            for bit in 0..8 {
                assert_eq!(
                    lc.output_enabled(&format!("io{bit}")),
                    Some(false),
                    "CE={ce_n} OE={oe_n} WE={we_n} must tri-state bit {bit}"
                );
            }
        }
    }

    #[test]
    fn comb_output_can_follow_an_interleaved_memory_output_in_the_same_tick() {
        let mut lc = compile(
            r#"
inputs = ["a0", "ce_n"]
outputs = ["io0", "copy"]

[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["a0"]
read_gates = [{ pin = "ce_n", active = "low" }]
data_out = ["io0"]

[comb]
copy = "io0"
"#,
        );
        tick_with(&mut lc, &[("a0", true), ("ce_n", false)]);
        assert_eq!(lc.output_level("io0"), Some(true));
        assert_eq!(
            lc.output_level("copy"),
            Some(true),
            "comb must see the current addressed word, not a prior-tick value"
        );
    }

    #[test]
    fn parallel_memory_commits_bus_on_qualified_write_edge_only() {
        let mut lc = compile(PARALLEL_EEPROM);
        let byte = 0x5a_u8;
        let mut levels = vec![
            ("a0", true),
            ("a1", true),
            ("ce_n", false),
            ("oe_n", true),
            ("we_n", false),
        ];
        for bit in 0..8 {
            levels.push((
                match bit {
                    0 => "io0",
                    1 => "io1",
                    2 => "io2",
                    3 => "io3",
                    4 => "io4",
                    5 => "io5",
                    6 => "io6",
                    _ => "io7",
                },
                byte & (1 << bit) != 0,
            ));
        }

        // WE low alone is not a commit. The rising edge captures address 3 and
        // the externally-driven bidirectional bus.
        tick_with(&mut lc, &levels);
        levels[4].1 = true;
        tick_with(&mut lc, &levels);

        tick_with(
            &mut lc,
            &[
                ("a0", true),
                ("a1", true),
                ("ce_n", false),
                ("oe_n", false),
                ("we_n", true),
            ],
        );
        for bit in 0..8 {
            assert_eq!(
                lc.output_level(&format!("io{bit}")),
                Some(byte & (1 << bit) != 0),
                "stored bit {bit}"
            );
        }

        // A second rising edge while CE is inactive must not overwrite it.
        tick_with(
            &mut lc,
            &[
                ("a0", true),
                ("a1", true),
                ("ce_n", true),
                ("oe_n", true),
                ("we_n", false),
                ("io0", true),
            ],
        );
        tick_with(
            &mut lc,
            &[
                ("a0", true),
                ("a1", true),
                ("ce_n", true),
                ("oe_n", true),
                ("we_n", true),
                ("io0", true),
            ],
        );
        tick_with(
            &mut lc,
            &[
                ("a0", true),
                ("a1", true),
                ("ce_n", false),
                ("oe_n", false),
                ("we_n", true),
            ],
        );
        let got = (0..8).fold(0u8, |acc, bit| {
            acc | (u8::from(lc.output_level(&format!("io{bit}")).unwrap()) << bit)
        });
        assert_eq!(got, byte, "CE-inactive edge must not write");
    }

    #[test]
    fn parallel_memory_accepts_declarative_we_and_ce_controlled_writes() {
        let mut lc = compile(
            r#"
inputs = ["a0", "ce_n", "oe_n", "we_n"]
outputs = ["io0"]
[[memory]]
name = "cell"
words = 2
bits = 1
init = 1
address = ["a0"]
write_cycles = [
  { pin = "we_n", edge = "rising", gates = [{ pin = "ce_n", active = "low" }] },
  { pin = "ce_n", edge = "rising", gates = [{ pin = "we_n", active = "low" }] },
]
read_gates = [
  { pin = "ce_n", active = "low" },
  { pin = "oe_n", active = "low" },
  { pin = "we_n", active = "high" },
]
data_in = ["io0"]
data_out = ["io0"]
"#,
        );

        // WE-controlled write to address 0.
        tick_with(
            &mut lc,
            &[
                ("a0", false),
                ("ce_n", false),
                ("oe_n", true),
                ("we_n", false),
                ("io0", false),
            ],
        );
        tick_with(
            &mut lc,
            &[
                ("a0", false),
                ("ce_n", false),
                ("oe_n", true),
                ("we_n", true),
                ("io0", false),
            ],
        );

        // CE-controlled write to address 1 while WE remains low.
        tick_with(
            &mut lc,
            &[
                ("a0", true),
                ("ce_n", false),
                ("oe_n", true),
                ("we_n", false),
                ("io0", false),
            ],
        );
        tick_with(
            &mut lc,
            &[
                ("a0", true),
                ("ce_n", true),
                ("oe_n", true),
                ("we_n", false),
                ("io0", false),
            ],
        );

        let port = lc.memory_ports().remove(0);
        assert_eq!(port.read(0), Some(0));
        assert_eq!(port.read(1), Some(0));
    }

    #[test]
    fn timed_page_program_honors_exact_load_boundary_and_busy_polling() {
        let lc = compile(
            r#"
inputs = ["a0", "a1", "ce_n", "oe_n", "we_n"]
outputs = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]
[[memory]]
name = "cell"
words = 4
bits = 8
page_words = 4
byte_load_timeout_s = 0.00015
program_time_s = 0.010
init = 0xff
address = ["a0", "a1"]
write = { pin = "we_n", edge = "rising" }
data_in = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]
data_out = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"]
"#,
        );
        let port = lc.memory_ports().remove(0);
        let hz = 1_000_000;

        assert!(port.write_at(0, 0x80, 1_000, hz));
        // Exactly 150 cycles is still part of the same page load.
        assert!(port.write_at(1, 0x81, 1_150, hz));
        assert_eq!(port.read_at(1, 2_000, hz), Some(0x7f));
        // I/O6 toggles between successive busy reads; I/O7 remains the
        // complement of the last loaded byte's I/O7.
        assert_eq!(port.read_at(1, 2_001, hz), Some(0x3f));
        assert_eq!(port.read_at(1, 11_149, hz), Some(0x7f));
        assert_eq!(port.read_at(1, 11_150, hz), Some(0x81));
        assert_eq!(port.read_at(0, 11_151, hz), Some(0x80));
        assert_eq!(port.read_at(1, 11_151, hz), Some(0x81));
    }

    #[test]
    fn timed_page_program_accepts_64_words_but_151_cycles_closes_the_page() {
        let address = (0..7).map(|bit| format!("a{bit}")).collect::<Vec<_>>();
        let data = (0..8).map(|bit| format!("io{bit}")).collect::<Vec<_>>();
        let mut inputs = address.clone();
        inputs.push("we_n".to_string());
        let spec = format!(
            r#"
inputs = [{}]
outputs = [{}]
[[memory]]
name = "cell"
words = 128
bits = 8
page_words = 64
byte_load_timeout_s = 0.00015
program_time_s = 0.010
init = 0xff
address = [{}]
write = {{ pin = "we_n", edge = "rising" }}
data_in = [{}]
data_out = [{}]
"#,
            inputs
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", "),
            data.iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", "),
            address
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", "),
            data.iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", "),
            data.iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let logic: Logic = toml::from_str(&spec).unwrap();
        let lc = LogicComponent::compile("page", &logic).unwrap();
        let port = lc.memory_ports().remove(0);
        for address in 0..64 {
            assert!(port.write_at(address, address as u64, 1_000 + address as u64, 1_000_000));
        }
        assert_eq!(port.read_at(63, 2_000, 1_000_000), Some(0xff));
        assert_eq!(port.read_at(63, 11_063, 1_000_000), Some(63));
        for address in 0..64 {
            assert_eq!(
                port.read_at(address, 11_064, 1_000_000),
                Some(address as u64)
            );
        }

        assert!(port.write_at(64, 0x11, 20_000, 1_000_000));
        // 151 µs is outside the inclusive load window, so this write arrives
        // while the previous word is already internally programming.
        assert!(port.write_at(65, 0x22, 20_151, 1_000_000));
        assert_eq!(port.read_at(64, 30_000, 1_000_000), Some(0x11));
        assert_eq!(port.read_at(65, 30_000, 1_000_000), Some(0xff));
    }

    #[test]
    fn protected_timed_write_attempt_enters_busy_without_changing_data() {
        let logic: Logic = toml::from_str(PARALLEL_EEPROM).unwrap();
        let mut logic = logic;
        logic.memories[0].program_time_s = Some(0.010);
        logic.memories[0]
            .software_data_protection
            .as_mut()
            .unwrap()
            .initial = true;
        let lc = LogicComponent::compile("protected", &logic).unwrap();
        let port = lc.memory_ports().remove(0);
        assert!(port.write_at(0, 0x00, 100, 1_000_000));
        assert_eq!(port.read_at(0, 101, 1_000_000), Some(0xff));
        assert_eq!(port.read_at(0, 102, 1_000_000), Some(0xbf));
        assert_eq!(port.read_at(0, 10_100, 1_000_000), Some(0xff));
    }

    #[test]
    fn parallel_memory_software_protection_consumes_commands_and_blocks_writes() {
        let mut lc = compile(PARALLEL_EEPROM);

        fn write_byte(lc: &mut LogicComponent, address: usize, byte: u8) {
            let names = ["io0", "io1", "io2", "io3", "io4", "io5", "io6", "io7"];
            let mut low = vec![
                ("a0", address & 1 != 0),
                ("a1", address & 2 != 0),
                ("ce_n", false),
                ("oe_n", true),
                ("we_n", false),
            ];
            for (bit, name) in names.iter().enumerate() {
                low.push((*name, byte & (1 << bit) != 0));
            }
            tick_with(lc, &low);
            low[4].1 = true;
            tick_with(lc, &low);
        }

        let port = lc.memory_ports().remove(0);
        write_byte(&mut lc, 0, 0x11);
        assert_eq!(port.read(0), Some(0x11));

        for (address, value) in [(3, 0xAA), (2, 0x55), (3, 0xA0)] {
            write_byte(&mut lc, address, value);
        }
        write_byte(&mut lc, 1, 0x42);
        assert_eq!(
            port.read(1),
            Some(0x42),
            "the enable sequence arms the following protected program cycle"
        );
        assert_eq!(port.read(2), Some(0xFF), "enable commands are not data");
        assert_eq!(port.read(3), Some(0xFF), "enable commands are not data");

        write_byte(&mut lc, 0, 0x22);
        assert_eq!(port.read(0), Some(0x11), "protected writes are ignored");

        // A near miss must neither disable protection nor leak command bytes
        // into storage.
        for (address, value) in [(3, 0xAA), (2, 0x55), (3, 0x81)] {
            write_byte(&mut lc, address, value);
        }
        write_byte(&mut lc, 0, 0x22);
        assert_eq!(port.read(0), Some(0x11));

        for (address, value) in [
            (3, 0xAA),
            (2, 0x55),
            (3, 0x80),
            (3, 0xAA),
            (2, 0x55),
            (3, 0x20),
        ] {
            write_byte(&mut lc, address, value);
        }
        write_byte(&mut lc, 0, 0x33);
        assert_eq!(port.read(0), Some(0x33), "exact disable restores writes");
        assert_eq!(port.read(2), Some(0xFF), "disable commands are not data");
        assert_eq!(port.read(3), Some(0xFF), "disable commands are not data");
    }

    #[test]
    fn protected_program_window_expires_after_the_declared_byte_load_gap() {
        let lc = compile(PARALLEL_EEPROM);
        let port = lc.memory_ports().remove(0);
        for (address, value) in [(3, 0xAA), (2, 0x55), (3, 0xA0)] {
            assert!(port.write(address, value));
        }

        assert!(port.write_after_gap(1, 0x42, true));
        assert_eq!(
            port.read(1),
            Some(0xFF),
            "a data write after the page-load timeout is protected"
        );
        assert_eq!(port.read(2), Some(0xFF), "command bytes remain consumed");
        assert_eq!(port.read(3), Some(0xFF), "command bytes remain consumed");
    }
}
