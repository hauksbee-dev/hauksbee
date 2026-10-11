//! How an extraction is paid for, and a subscription run's share of its plan.
//!
//! A CLI signed in with a subscription (Claude Code on a Claude plan, codex on
//! a `ChatGPT` plan) has no per-call charge, so a dollar figure would mislead:
//! the run is reported as the change in the plan's usage limits instead (the
//! weekly percentage, and the 5-hour window), read from the CLI before and
//! after. Billing is worked out from how the CLI is signed in, never assumed;
//! when that cannot be told it is "billing unknown" and no dollars are shown.
//! A reading that fails is "usage unavailable", never a guessed percentage.

use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::extract_usage::Billing;

/// A plan's usage, in percent of each limit window (`None`: not reported).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Reading {
    /// The weekly limit.
    pub week: Option<f64>,
    /// The 5-hour window.
    pub session: Option<f64>,
}

impl Reading {
    const fn any(self) -> bool {
        self.week.is_some() || self.session.is_some()
    }
}

fn spawn(command: &str, args: &[&str]) -> Option<Child> {
    Command::new(command)
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// A command's stdout, or `None` if it fails to start or outlives `secs` (killed).
fn output(command: &str, args: &[&str], secs: u64) -> Option<String> {
    let mut child = spawn(command, args)?;
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(secs);
    while child.try_wait().ok()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    Some(text)
}

/// How a Claude Code CLI bills: an `ANTHROPIC_API_KEY` or an API-key sign-in
/// is metered, a claude.ai sign-in draws on the plan, anything else is unknown.
#[must_use]
pub fn claude_billing(command: &str) -> Billing {
    let key = std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.trim().is_empty());
    if key {
        return Billing::Metered { fast: false };
    }
    let status = output(command, &["auth", "status", "--json"], 10);
    let status: Value = status
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    billing_from_claude_status(&status)
}

/// [`claude_billing`] from `claude auth status --json` alone.
#[must_use]
pub fn billing_from_claude_status(status: &Value) -> Billing {
    let method = status["authMethod"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if status["loggedIn"] != true {
        Billing::Unknown
    } else if method == "claude.ai" {
        Billing::Plan
    } else if method.contains("api") || method.contains("console") {
        Billing::Metered { fast: false }
    } else {
        Billing::Unknown
    }
}

/// The plan's usage now, from the CLI that runs `tool`; `None` when it cannot be read.
#[must_use]
pub fn read(tool: &str, command: &str) -> Option<Reading> {
    let reading = match tool {
        "claude-code" => output(command, &["-p", "/usage"], 60).map(|t| parse_claude_usage(&t)),
        "codex" => read_codex(command).map(|v| parse_codex_limits(&v)),
        _ => None,
    };
    reading.filter(|r| r.any())
}

/// `claude -p /usage` text: "Current session: 4% used", "Current week (all models): 46% used".
#[must_use]
pub fn parse_claude_usage(text: &str) -> Reading {
    let pct = |prefix: &str| {
        let rest = text.lines().find_map(|l| l.trim().strip_prefix(prefix))?;
        rest.split('%').next()?.trim().parse().ok()
    };
    Reading {
        week: pct("Current week (all models):"),
        session: pct("Current session:"),
    }
}

/// codex's own rate limits, from `codex app-server`'s `account/rateLimits/read`
/// (no model call). Killed after 20 s.
fn read_codex(command: &str) -> Option<Value> {
    let mut child = spawn(command, &["app-server"])?;
    let mut stdin = child.stdin.take()?;
    let lines = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"clientInfo": {"name": "hauksbee", "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc": "2.0", "method": "initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read"}),
    ];
    for l in lines {
        writeln!(stdin, "{l}").ok()?;
    }
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let v: Value = serde_json::from_str(&line).unwrap_or_default();
            if v["id"] == 2 {
                let _ = tx.send(v);
                return;
            }
        }
    });
    let reply = rx.recv_timeout(Duration::from_secs(20)).ok();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    reply.map(|v| v["result"]["rateLimits"].clone())
}

/// codex `rateLimits`: the window of 300 minutes is the 5-hour one, 10080 the week.
#[must_use]
pub fn parse_codex_limits(limits: &Value) -> Reading {
    let mut r = Reading::default();
    for w in [&limits["primary"], &limits["secondary"]] {
        let pct = w["usedPercent"].as_f64();
        match w["windowDurationMins"].as_u64() {
            Some(300) => r.session = pct,
            Some(10080) => r.week = pct,
            _ => {}
        }
    }
    r
}

/// A plan run's tool, CLI command and the reading taken before its first call.
type Pending = (String, String, Option<Reading>);

static PENDING: std::sync::Mutex<Option<Pending>> = std::sync::Mutex::new(None);

fn pending() -> std::sync::MutexGuard<'static, Option<Pending>> {
    PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Before an extraction's first plan-billed call: read the plan once, so the
/// whole extraction (kind identification included) is measured by [`end`].
pub fn begin(tool: &str, command: &str) {
    if pending().is_none() {
        let before = read(tool, command);
        *pending() = Some((tool.to_string(), command.to_string(), before));
    }
}

/// Drops a reading left by an extraction that never reached [`end`].
pub fn clear() {
    *pending() = None;
}

/// After the extraction: read the plan again and report the change (`None`
/// when no plan-billed call was made). Clears the pending reading.
#[must_use]
pub fn end() -> Option<String> {
    let (tool, command, before) = pending().take()?;
    let after = read(&tool, &command);
    Some(report(&tool, before, after))
}

/// The run's share of the plan, as one line and a `plan` activity event, from
/// readings before and after (`None`: unavailable). Only for a plan run.
#[must_use]
pub fn report(tool: &str, before: Option<Reading>, after: Option<Reading>) -> String {
    let plan = if tool == "codex" {
        "ChatGPT plan's Codex limits"
    } else {
        "Claude plan's limits"
    };
    let (Some(b), Some(a)) = (before, after) else {
        crate::extract_progress::event("plan", json!({"tool": tool, "available": false}));
        return format!("plan usage: usage unavailable (could not read the {plan})");
    };
    let window = |name: &str, b: Option<f64>, a: Option<f64>| {
        b.zip(a)
            .map(|(b, a)| format!("{name} {b:.0}% -> {a:.0}% ({:+.0} points)", a - b))
    };
    let parts: Vec<String> = [
        window("week", b.week, a.week),
        window("5-hour", b.session, a.session),
    ]
    .into_iter()
    .flatten()
    .collect();
    crate::extract_progress::event(
        "plan",
        json!({"tool": tool, "available": true,
               "before": {"week": b.week, "session": b.session},
               "after": {"week": a.week, "session": a.session}}),
    );
    format!("plan usage ({plan}): {}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_readings_and_billing_are_parsed_from_the_cli_output() {
        let text = "Current session: 4% used · resets 1am\nCurrent week (all models): 46% used · resets\nCurrent week (Fable): 0% used";
        let r = parse_claude_usage(text);
        assert_eq!((r.week, r.session), (Some(46.0), Some(4.0)));
        assert!(!parse_claude_usage("Error: not signed in").any());
        let limits = json!({"primary": {"usedPercent": 14, "windowDurationMins": 300},
                            "secondary": {"usedPercent": 24.5, "windowDurationMins": 10080}});
        let r = parse_codex_limits(&limits);
        assert_eq!((r.week, r.session), (Some(24.5), Some(14.0)));
        let status = |m: &str| json!({"loggedIn": true, "authMethod": m});
        assert_eq!(
            billing_from_claude_status(&status("claude.ai")),
            Billing::Plan
        );
        assert_eq!(
            billing_from_claude_status(&status("api_key")),
            Billing::Metered { fast: false }
        );
        assert_eq!(
            billing_from_claude_status(&status("oauth?")),
            Billing::Unknown
        );
        assert_eq!(billing_from_claude_status(&Value::Null), Billing::Unknown);
    }

    #[test]
    fn a_missing_reading_is_reported_unavailable_never_guessed() {
        let r = Reading {
            week: Some(46.0),
            session: Some(4.0),
        };
        assert!(report("claude-code", None, Some(r)).contains("usage unavailable"));
        let after = Reading {
            week: Some(47.0),
            session: None,
        };
        let line = report("claude-code", Some(r), Some(after));
        assert_eq!(
            line,
            "plan usage (Claude plan's limits): week 46% -> 47% (+1 points)"
        );
    }
}
