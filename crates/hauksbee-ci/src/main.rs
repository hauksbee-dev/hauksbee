//! `hauksbee-ci` CLI.
//!
//! ```text
//! hauksbee-ci run <spec.toml> [--junit out.xml] [--quiet]
//! ```
//!
//! Exit code is 0 if every assertion passed, 1 otherwise (2 on a usage/spec
//! error). When `GITHUB_ACTIONS` is set in the environment, GitHub workflow
//! annotations are emitted to stdout so failures surface inline.

use std::path::PathBuf;
use std::process::ExitCode;

use hauksbee_ci::{run, RunConfig};

struct Args {
    spec: PathBuf,
    junit: Option<PathBuf>,
    quiet: bool,
}

fn usage() -> String {
    "usage: hauksbee-ci run <spec.toml> [--junit <out.xml>] [--quiet]".to_string()
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or_else(usage)?;
    if cmd == "--help" || cmd == "-h" {
        return Err(usage());
    }
    if cmd != "run" {
        return Err(format!("unknown command '{cmd}'\n{}", usage()));
    }
    let spec = it
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing spec file\n{}", usage()))?;
    let mut args = Args {
        spec,
        junit: None,
        quiet: false,
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--junit" => {
                args.junit = Some(PathBuf::from(
                    it.next().ok_or("--junit needs a path")?,
                ))
            }
            "--quiet" => args.quiet = true,
            other => return Err(format!("unknown flag '{other}'\n{}", usage())),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let cfg = RunConfig {
        spec: args.spec.clone(),
    };
    let result = match run(&cfg) {
        Ok(r) => r,
        Err(e) => {
            // Spec / board errors: surface as a GitHub error too, then exit 2.
            eprintln!("hauksbee-ci: {e}");
            if std::env::var_os("GITHUB_ACTIONS").is_some() {
                // Percent first, then control chars (else the %0A/%0D we insert
                // get their own % re-encoded to %25, garbling the annotation).
                let msg = e
                    .to_string()
                    .replace('%', "%25")
                    .replace('\r', "%0D")
                    .replace('\n', "%0A");
                println!("::error title=hauksbee-ci spec error::{msg}");
            }
            return ExitCode::from(2);
        }
    };

    if !args.quiet {
        print!("{}", result.render_human());
    }

    if let Some(path) = &args.junit {
        if let Err(e) = std::fs::write(path, result.render_junit()) {
            eprintln!("hauksbee-ci: could not write JUnit XML to {}: {e}", path.display());
            return ExitCode::from(2);
        }
        if !args.quiet {
            println!("wrote JUnit XML to {}", path.display());
        }
    }

    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        print!("{}", result.render_github_annotations());
    }

    ExitCode::from(result.exit_code() as u8)
}
