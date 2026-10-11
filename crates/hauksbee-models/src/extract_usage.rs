//! What an extraction cost: tokens per backend call and an estimated list price.
//!
//! Only some backends say. `codex exec --json` ends each turn with a
//! `turn.completed` event carrying token counts, an OpenAI-compatible API
//! reply has a `usage` object, and `claude -p --output-format stream-json` ends
//! with a `result` event whose `modelUsage` gives tokens and a list-price cost
//! per model; `agy` prints none. A call whose usage is unknown is reported as unknown and never counted
//! as zero, so a total over some unknown calls is a floor, and says so.
//!
//! Prices are USD per million tokens (input, cached input, output) at the
//! standard list tier, kept for the models this tool has a source for.
//! A codex CLI signed in with a `ChatGPT` account draws on the plan the user
//! already pays for, so its calls show tokens and no dollar figure; dollars are
//! only estimated for metered calls (an API key, or the `api` backend).
//! A metered call on the fast tier (`service_tier="fast"`, or its former name
//! `"priority"`) bills double. `HAUKSBEE_LLM_PRICE=in,cached,out` prices any
//! other model as given, tier included; with neither, the tokens are shown and
//! the cost is unknown.

use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;

/// Token counts for one call. `input` includes the `cached` part, as `OpenAI` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

/// USD per million tokens: input, cached input, output.
type Price = [f64; 3];

/// Standard-tier list prices, from developers.openai.com/api/docs/pricing
/// (2026-10-06); the fast tier is listed at exactly double.
const LIST_PRICES: &[(&str, Price)] = &[
    ("gpt-6.1-sol", [2.00, 0.10, 10.00]),
    ("gpt-6-luna", [0.10, 0.01, 0.50]),
];

/// How a call is paid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Billing {
    /// Billed per token at list price; `fast` doubles it.
    Metered { fast: bool },
    /// Drawn from a subscription plan (`ChatGPT` for codex, Claude for Claude
    /// Code): no per-call charge, so no dollar figure.
    Plan,
    /// How the CLI is signed in could not be told: no dollar figure either.
    Unknown,
}

impl Billing {
    /// The activity-event name: `metered`, `plan` or `unknown`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Metered { .. } => "metered",
            Self::Plan => "plan",
            Self::Unknown => "unknown",
        }
    }
}

/// How a codex CLI bills: `codex login status` names a `ChatGPT` sign-in
/// (plan) or an API key (metered); anything else is unknown.
#[must_use]
pub fn codex_billing(command: &str, extra_args: &[String]) -> Billing {
    let status = login_status(command).unwrap_or_default();
    if status.contains("ChatGPT") {
        Billing::Plan
    } else if status.to_ascii_lowercase().contains("api key") {
        Billing::Metered {
            fast: fast_tier(extra_args),
        }
    } else {
        Billing::Unknown
    }
}

/// `codex login status` output, or `None` if it fails or takes over 5 s (it is killed).
fn login_status(command: &str) -> Option<String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new(command)
        .args(["login", "status"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while child.try_wait().ok()?.is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    child.stderr.take()?.read_to_string(&mut text).ok()?;
    Some(text)
}

/// Whether backend arguments select `OpenAI`'s fast tier (named "priority" before 2026-07-30).
#[must_use]
pub fn fast_tier(args: &[String]) -> bool {
    args.iter().any(|a| {
        let a = a.replace(['"', '\'', ' '], "");
        a.ends_with("service_tier=fast") || a.ends_with("service_tier=priority")
    })
}

#[derive(Default, Clone)]
struct Ledger {
    calls: usize,
    known: usize,
    tokens: Usage,
    /// Summed cost of the known metered calls; `None` once any has no price.
    cost: Option<f64>,
    /// Known calls drawn from a plan (or of unknown billing) rather than metered.
    plan: usize,
    /// Where the non-metered calls' usage went (see `source`).
    source: Option<&'static str>,
    /// The run's share of a subscription plan, once read (see `extract_plan`).
    plan_note: Option<String>,
}

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);

fn ledger() -> MutexGuard<'static, Option<Ledger>> {
    LEDGER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A new extraction: forget the previous one's calls.
pub fn reset() {
    *ledger() = None;
}

/// Attach the run's plan-usage line (see `extract_plan::report`) to the summary.
pub fn note_plan(line: String) {
    ledger().get_or_insert_with(Ledger::default).plan_note = Some(line);
}

/// The summed `turn.completed` usage in a codex `--json` stream (`None` when it has none).
#[must_use]
pub fn from_codex_events(stdout: &str) -> Option<Usage> {
    let turns = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let turns = turns.filter(|e| e["type"] == "turn.completed");
    turns
        .filter_map(|e| usage_object(&e["usage"], "input_tokens", "output_tokens"))
        .reduce(add)
}

/// Per-model tokens, and Claude Code's own list-price cost, from its
/// `stream-json` `result` event.
///
/// The cost is kept only for a first-party Claude
/// model it prices at list (`costBasis: "list"`): a gateway model routed
/// through Claude Code is priced from its tokens here instead, because Claude
/// Code prices names it does not know at Opus-class rates.
#[must_use]
pub fn from_claude_result(result: &Value) -> Vec<(String, Usage, Option<f64>)> {
    let models = result["modelUsage"].as_object().into_iter().flatten();
    models
        .map(|(name, m)| {
            let n = |k: &str| m[k].as_u64().unwrap_or(0);
            let cached = n("cacheReadInputTokens");
            let input = n("inputTokens") + cached + n("cacheCreationInputTokens");
            let listed = name.starts_with("claude-") && m["costBasis"] == "list";
            let usage = Usage {
                input,
                cached,
                output: n("outputTokens"),
            };
            (
                name.clone(),
                usage,
                m["costUSD"].as_f64().filter(|_| listed),
            )
        })
        .collect()
}

/// The `usage` of a chat-completions reply.
#[must_use]
pub fn from_chat_reply(reply: &Value) -> Option<Usage> {
    usage_object(&reply["usage"], "prompt_tokens", "completion_tokens")
}

fn usage_object(u: &Value, input: &str, output: &str) -> Option<Usage> {
    let count = |v: &Value| v.as_u64();
    let cached = count(&u["cached_input_tokens"])
        .or_else(|| count(&u["prompt_tokens_details"]["cached_tokens"]));
    Some(Usage {
        input: count(&u[input])?,
        cached: cached.unwrap_or(0),
        output: count(&u[output])?,
    })
}

const fn add(a: Usage, b: Usage) -> Usage {
    Usage {
        input: a.input + b.input,
        cached: a.cached + b.cached,
        output: a.output + b.output,
    }
}

fn price(model: &str, fast: bool) -> Option<Price> {
    if let Ok(text) = std::env::var("HAUKSBEE_LLM_PRICE") {
        let parts: Vec<f64> = text
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        return <Price>::try_from(parts).ok();
    }
    let tier = if fast { 2.0 } else { 1.0 };
    LIST_PRICES
        .iter()
        .find(|(m, _)| *m == model)
        .map(|(_, p)| p.map(|x| x * tier))
}

#[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
fn cost(u: Usage, p: Price) -> f64 {
    let counts = [u.input.saturating_sub(u.cached), u.cached, u.output];
    counts
        .iter()
        .zip(p)
        .map(|(n, per_m)| *n as f64 / 1e6 * per_m)
        .sum()
}

fn describe(u: Usage) -> String {
    format!(
        "{} input ({} cached), {} output tokens",
        u.input, u.cached, u.output
    )
}

/// Record one backend call (`None` = it reported no usage) and say what it cost.
pub fn record(tool: &str, model: &str, billing: Billing, usage: Option<Usage>) {
    record_priced(tool, model, billing, usage, None);
}

/// [`record`], with the list-price cost the backend itself reported for a
/// metered call (`None`: price it from [`LIST_PRICES`]). Also sends a `usage`
/// activity event carrying the run's totals so far.
pub fn record_priced(
    tool: &str,
    model: &str,
    billing: Billing,
    usage: Option<Usage>,
    reported: Option<f64>,
) {
    let line = ledger_line(tool, model, billing, usage, reported);
    crate::extract_progress::say(&format!("[model-extract] {line}"));
    let (u, l) = (
        usage.unwrap_or_default(),
        ledger().clone().unwrap_or_default(),
    );
    let call = usage.and_then(|u| reported.or_else(|| price(model, false).map(|p| cost(u, p))));
    let call = call.filter(|_| matches!(billing, Billing::Metered { .. }));
    crate::extract_progress::event(
        "usage",
        serde_json::json!({
            "model": model, "billing": billing.name(),
            "input": u.input, "output": u.output, "cache_read": u.cached,
            "cost_usd": call, "total_tokens": l.tokens.input + l.tokens.output,
            "total_cost_usd": l.cost.filter(|_| l.plan < l.known),
        }),
    );
}

fn ledger_line(
    tool: &str,
    model: &str,
    billing: Billing,
    usage: Option<Usage>,
    reported: Option<f64>,
) -> String {
    let mut guard = ledger();
    let l = guard.get_or_insert_with(|| Ledger {
        cost: Some(0.0),
        ..Ledger::default()
    });
    l.calls += 1;
    let line = match usage {
        None => format!(
            "{tool} call {}: tokens and cost unknown ({tool} did not report usage)",
            l.calls
        ),
        Some(u) => {
            l.known += 1;
            l.tokens = add(l.tokens, u);
            let Billing::Metered { fast } = billing else {
                l.plan += 1;
                let from = source(tool, billing);
                l.source = Some(
                    l.source
                        .filter(|s| *s != from)
                        .map_or(from, |_| source("", Billing::Unknown)),
                );
                return format!("{tool} call {}: {}, {from}", l.calls, describe(u));
            };
            let priced = reported.or_else(|| price(model, fast).map(|p| cost(u, p)));
            l.cost = l.cost.zip(priced).map(|(a, b)| a + b);
            let spend = priced.map_or_else(
                || format!("cost unknown (no list price for {model}; set HAUKSBEE_LLM_PRICE=in,cached,out in USD per million tokens)"),
                |c| format!("about ${c:.4} at list price"),
            );
            format!("{tool} call {}: {}, {spend}", l.calls, describe(u))
        }
    };
    drop(guard);
    line
}

/// Where a non-metered call's usage goes.
fn source(tool: &str, billing: Billing) -> &'static str {
    match (billing, tool) {
        (Billing::Plan, "codex") => "from your ChatGPT plan's Codex usage (no per-call charge)",
        (Billing::Plan, _) => "from your Claude plan's usage (no per-call charge)",
        _ => "billing unknown (could not tell an API key from a plan sign-in; no dollar figure)",
    }
}

/// The one-line cost of the whole extraction so far (empty before any call),
/// with the plan-usage line after it for a subscription run.
#[must_use]
pub fn summary() -> String {
    let Some(l) = ledger().clone() else {
        return String::new();
    };
    let line = summary_of(&l);
    match l.plan_note {
        Some(note) if !line.is_empty() => format!("{line}; {note}"),
        Some(note) => note,
        None => line,
    }
}

fn summary_of(l: &Ledger) -> String {
    if l.calls == 0 {
        return String::new();
    }
    if l.known == 0 {
        return format!(
            "cost: unknown ({} call(s), none reported token usage)",
            l.calls
        );
    }
    if l.plan == l.known {
        let part = if l.known < l.calls {
            format!(", {} of {} call(s) reported usage", l.known, l.calls)
        } else {
            String::new()
        };
        let from = l.source.unwrap_or_default();
        return format!("usage: {}, {from}{part}", describe(l.tokens));
    }
    let money = l.cost.map_or_else(
        || "cost unknown".to_string(),
        |c| {
            format!(
                "{}${c:.4} at list price",
                if l.known < l.calls {
                    "at least "
                } else {
                    "about "
                }
            )
        },
    );
    let part = if l.known < l.calls {
        format!(", {} of {} call(s) reported usage", l.known, l.calls)
    } else {
        String::new()
    };
    format!("cost: {}, {money}{part}", describe(l.tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    const METERED: Billing = Billing::Metered { fast: false };

    /// The ledger is process-wide, so tests that touch it run one at a time.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    const STREAM: &str = concat!(
        "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\n",
        "not json\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1000000,\"cached_input_tokens\":400000,\"output_tokens\":100000}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":500000,\"cached_input_tokens\":0,\"output_tokens\":0,\"reasoning_output_tokens\":0}}\n",
    );

    #[test]
    fn codex_turns_sum_and_a_stream_without_usage_is_unknown() {
        let u = from_codex_events(STREAM).unwrap();
        assert_eq!(
            u,
            Usage {
                input: 1_500_000,
                cached: 400_000,
                output: 100_000
            }
        );
        assert_eq!(
            from_codex_events("{\"type\":\"turn.started\"}\ngarbage"),
            None
        );
        let chat = serde_json::json!({"usage": {"prompt_tokens": 7, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 2}}});
        assert_eq!(
            from_chat_reply(&chat),
            Some(Usage {
                input: 7,
                cached: 2,
                output: 3
            })
        );
        assert_eq!(from_chat_reply(&serde_json::json!({})), None);
    }

    #[test]
    fn unknown_is_never_zero_and_partial_totals_are_a_floor() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::remove_var("HAUKSBEE_LLM_PRICE");
        reset();
        assert_eq!(summary(), "");
        record("claude-code", "m", METERED, None);
        assert!(summary().starts_with("cost: unknown"), "{}", summary());
        reset();
        let u = from_codex_events(STREAM).unwrap();
        record("codex", "gpt-6-luna", METERED, Some(u));
        // fresh 1.1M * 0.10 + cached 0.4M * 0.01 + out 0.1M * 0.50, per million.
        assert_eq!(summary(), "cost: 1500000 input (400000 cached), 100000 output tokens, about $0.1640 at list price");
        record("codex", "gpt-6-luna", METERED, None);
        let s = summary();
        assert!(
            s.contains("at least $0.1640") && s.contains("1 of 2 call(s) reported usage"),
            "{s}"
        );
        reset();
        // Sol: 1.1M * 2.00 + 0.4M * 0.10 + 0.1M * 10.00; the fast tier doubles it.
        record("codex", "gpt-6.1-sol", METERED, Some(u));
        assert!(summary().contains("about $3.2400"), "{}", summary());
        reset();
        let fast = ["-c".to_string(), "service_tier=\"priority\"".to_string()];
        record(
            "codex",
            "gpt-6.1-sol",
            Billing::Metered {
                fast: fast_tier(&fast),
            },
            Some(u),
        );
        assert!(summary().contains("about $6.4800"), "{}", summary());
        assert!(fast_tier(&["service_tier=fast".to_string()]));
        assert!(!fast_tier(&["service_tier=\"flex\"".to_string()]));
        reset();
        // A ChatGPT-plan call shows tokens, never dollars.
        record("codex", "gpt-6.1-sol", Billing::Plan, Some(u));
        let s = summary();
        assert!(
            s.starts_with("usage: 1500000 input") && s.contains("ChatGPT plan") && !s.contains('$'),
            "{s}"
        );
        reset();
        record("codex", "no-such-model", METERED, Some(u));
        assert!(summary().contains("cost unknown"), "{}", summary());
        std::env::set_var("HAUKSBEE_LLM_PRICE", "1,0,2");
        reset();
        record("codex", "no-such-model", METERED, Some(u));
        assert!(summary().contains("about $1.3000"), "{}", summary());
        std::env::remove_var("HAUKSBEE_LLM_PRICE");
        reset();
    }
}
