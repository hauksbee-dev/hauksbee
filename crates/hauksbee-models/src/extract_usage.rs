//! What an extraction cost: tokens per backend call and an estimated list price.
//!
//! Only some backends say. `codex exec --json` ends each turn with a
//! `turn.completed` event carrying token counts, and an OpenAI-compatible API
//! reply has a `usage` object; `claude -p --output-format text` and `agy` print
//! none. A call whose usage is unknown is reported as unknown and never counted
//! as zero, so a total over some unknown calls is a floor, and says so.
//!
//! Prices are USD per million tokens (input, cached input, output) at the
//! standard list tier, kept for the models this tool has a source for.
//! `HAUKSBEE_LLM_PRICE=in,cached,out` prices any other model; with neither, the
//! tokens are shown and the cost is unknown.

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

/// Standard-tier list prices; the fast tier bills double.
const LIST_PRICES: &[(&str, Price)] = &[("gpt-6-luna", [0.10, 0.01, 0.50])];

#[derive(Default, Clone)]
struct Ledger {
    calls: usize,
    known: usize,
    tokens: Usage,
    /// Summed cost of the known calls; `None` once any known call has no price.
    cost: Option<f64>,
}

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);

fn ledger() -> MutexGuard<'static, Option<Ledger>> {
    LEDGER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A new extraction: forget the previous one's calls.
pub fn reset() {
    *ledger() = None;
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

fn price(model: &str) -> Option<Price> {
    if let Ok(text) = std::env::var("HAUKSBEE_LLM_PRICE") {
        let parts: Vec<f64> = text
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        return <Price>::try_from(parts).ok();
    }
    LIST_PRICES
        .iter()
        .find(|(m, _)| *m == model)
        .map(|(_, p)| *p)
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
pub fn record(tool: &str, model: &str, usage: Option<Usage>) {
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
            let priced = price(model).map(|p| cost(u, p));
            l.cost = l.cost.zip(priced).map(|(a, b)| a + b);
            let spend = priced.map_or_else(
                || format!("cost unknown (no list price for {model}; set HAUKSBEE_LLM_PRICE=in,cached,out in USD per million tokens)"),
                |c| format!("about ${c:.4} at list price"),
            );
            format!("{tool} call {}: {}, {spend}", l.calls, describe(u))
        }
    };
    drop(guard);
    crate::extract_progress::say(&format!("[model-extract] {line}"));
}

/// The one-line cost of the whole extraction so far (empty before any call).
#[must_use]
pub fn summary() -> String {
    let Some(l) = ledger().clone() else {
        return String::new();
    };
    if l.known == 0 {
        return format!(
            "cost: unknown ({} call(s), none reported token usage)",
            l.calls
        );
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
        record("claude-code", "m", None);
        assert!(summary().starts_with("cost: unknown"), "{}", summary());
        reset();
        let u = from_codex_events(STREAM).unwrap();
        record("codex", "gpt-6-luna", Some(u));
        // fresh 1.1M * 0.10 + cached 0.4M * 0.01 + out 0.1M * 0.50, per million.
        assert_eq!(summary(), "cost: 1500000 input (400000 cached), 100000 output tokens, about $0.1640 at list price");
        record("codex", "gpt-6-luna", None);
        let s = summary();
        assert!(
            s.contains("at least $0.1640") && s.contains("1 of 2 call(s) reported usage"),
            "{s}"
        );
        reset();
        record("codex", "no-such-model", Some(u));
        assert!(summary().contains("cost unknown"), "{}", summary());
        std::env::set_var("HAUKSBEE_LLM_PRICE", "1,0,2");
        reset();
        record("codex", "no-such-model", Some(u));
        assert!(summary().contains("about $1.3000"), "{}", summary());
        std::env::remove_var("HAUKSBEE_LLM_PRICE");
        reset();
    }
}
