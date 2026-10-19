//! Running the parts of a `models extract --unmodelled` batch: several at once
//! on a bounded pool, one readable line at a time, and one honest tally.
//!
//! A whole board is dozens of parts, each a web search, a download and an
//! agent run that is mostly waiting, so they run side by side (`--jobs N`,
//! default [`DEFAULT_JOBS`]); `--jobs 1` is the plain serial loop with the
//! spinner. Each part is its own unit of work and ends in exactly one of three
//! ways, and the tally counts them apart:
//!
//! - **drafted**: a card was written;
//! - **no datasheet**: none could be obtained (nothing was sent), listed at the
//!   end for the person to supply;
//! - **failed**: the extraction itself ran and was refused (the model, the
//!   validator). Only these make the command exit non-zero.
//!
//! Safety of the shared output library: two parts whose card (and downloaded
//! datasheet) would be the same file name run one after the other in the same
//! lane, never at once, and every other part writes only its own files.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hauksbee_ir::{eprintln, println};
use hauksbee_models::datasheet::{self, Args};
use hauksbee_models::datasheet_discovery::{Acquirer, Missing, Outcome, Wanted};
use hauksbee_models::extract_messages::{self as words, Need};
use hauksbee_models::extract_progress as progress;

use super::models::UnmodelledPart;

/// How many parts are worked on at once unless `--jobs` says otherwise.
pub const DEFAULT_JOBS: usize = 4;

/// The most parts worked on at once: past this a batch hits the backend's and
/// the vendors' rate limits long before it gets faster.
pub const MAX_JOBS: usize = 16;

/// How a batch is run.
#[derive(Debug, Clone)]
pub struct BatchOptions {
    /// Parts worked on at once (`--jobs`); 1 is the serial loop.
    pub jobs: usize,
    /// A folder of PDFs named `<part>.pdf` to use instead of fetching
    /// (`--datasheet-dir`). `None` uses `<out-dir>/datasheets`, where fetched
    /// ones are kept, so the same folder serves both.
    pub datasheet_dir: Option<PathBuf>,
    /// How often a quiet stretch of a long batch says how far it has got.
    pub progress_every: Duration,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            jobs: DEFAULT_JOBS,
            datasheet_dir: None,
            progress_every: Duration::from_secs(30),
        }
    }
}

/// What a batch did, part by part.
#[derive(Debug, Default)]
pub struct Tally {
    pub drafted: usize,
    /// Parts nothing could be fetched for, with every link tried.
    pub no_datasheet: Vec<(String, Missing)>,
    /// Parts whose extraction ran and failed, with the error.
    pub failed: Vec<(String, String)>,
    /// Questions an agent asked that nobody was there to answer.
    pub needs: Vec<Need>,
}

impl Tally {
    /// The names of the failed parts.
    #[must_use]
    pub fn failed_names(&self) -> Vec<&str> {
        self.failed.iter().map(|(p, _)| p.as_str()).collect()
    }
}

/// How one part ended.
enum Ended {
    Drafted,
    NoDatasheet(Missing),
    Failed(String),
}

struct PartRun {
    ended: Ended,
    needs: Vec<String>,
}

/// Runs one extraction (the real one is [`datasheet::run`] under the spinner).
pub type Extract<'a> = &'a (dyn Fn(Args) -> anyhow::Result<PathBuf> + Sync);

/// Lines from several parts, each whole and each saying whose it is.
struct Console {
    prefixed: bool,
}

/// One message at a time, so a multi-line message stays together.
static PRINT: Mutex<()> = Mutex::new(());

impl Console {
    fn say(&self, part: &str, text: &str) {
        self.emit(part, text, false);
    }

    fn complain(&self, part: &str, text: &str) {
        self.emit(part, text, true);
    }

    fn emit(&self, part: &str, text: &str, to_err: bool) {
        let _one_at_a_time = PRINT.lock().unwrap_or_else(PoisonError::into_inner);
        for line in text.lines() {
            // Most lines already lead with the part; a prefix would only repeat it.
            let lead: String = line.chars().take(part.chars().count() + 24).collect();
            let line = if self.prefixed && !lead.contains(part) {
                format!("[{part}] {line}")
            } else {
                line.to_string()
            };
            if to_err {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }
}

/// Group the parts that would write the same file (a card, its downloaded
/// datasheet, its rejected draft) so one worker takes them in turn.
fn lanes(work: &[&UnmodelledPart]) -> Vec<Vec<usize>> {
    let mut lanes: Vec<(String, Vec<usize>)> = Vec::new();
    for (at, p) in work.iter().enumerate() {
        let key = datasheet::sanitise_filename(&p.part).to_ascii_lowercase();
        match lanes.iter_mut().find(|(k, _)| *k == key) {
            Some((_, lane)) => lane.push(at),
            None => lanes.push((key, vec![at])),
        }
    }
    lanes.into_iter().map(|(_, lane)| lane).collect()
}

/// Run `job(0..count)` on up to `jobs` threads, every lane (indexes that must
/// not overlap) on one thread in order, and return each result by index. A job
/// that panics leaves `None` and the rest carry on. While it runs,
/// `announce(done, count, running)` is called whenever `every` passes without
/// one having been made.
fn schedule<T: Send>(
    count: usize,
    lanes: Vec<Vec<usize>>,
    jobs: usize,
    every: Duration,
    job: &(dyn Fn(usize) -> T + Sync),
    announce: &(dyn Fn(usize, usize, usize) + Sync),
) -> Vec<Option<T>> {
    let slots: Vec<Mutex<Option<T>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let workers = jobs.clamp(1, MAX_JOBS).min(lanes.len().max(1));
    if workers == 1 {
        for at in lanes.into_iter().flatten() {
            let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(at)));
            *slots[at].lock().unwrap_or_else(PoisonError::into_inner) = made.ok();
        }
        return slots
            .into_iter()
            .map(|s| s.into_inner().unwrap_or_else(PoisonError::into_inner))
            .collect();
    }
    let queue = Mutex::new(VecDeque::from(lanes));
    let (done, finished) = (Mutex::new(0usize), Condvar::new());
    let running = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let lane = queue
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .pop_front();
                let Some(lane) = lane else { break };
                for at in lane {
                    running.fetch_add(1, Ordering::SeqCst);
                    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(at)));
                    *slots[at].lock().unwrap_or_else(PoisonError::into_inner) = made.ok();
                    running.fetch_sub(1, Ordering::SeqCst);
                    *done.lock().unwrap_or_else(PoisonError::into_inner) += 1;
                    finished.notify_all();
                }
            });
        }
        let mut last = Instant::now();
        let mut now = done.lock().unwrap_or_else(PoisonError::into_inner);
        while *now < count {
            let left = every.saturating_sub(last.elapsed());
            now = finished
                .wait_timeout(now, left.max(Duration::from_millis(1)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            if *now < count && last.elapsed() >= every {
                announce(*now, count, running.load(Ordering::SeqCst));
                last = Instant::now();
            }
        }
    });
    slots
        .into_iter()
        .map(|s| s.into_inner().unwrap_or_else(PoisonError::into_inner))
        .collect()
}

/// Get one part's datasheet and draft its card.
fn run_part(
    p: &UnmodelledPart,
    acquirer: &Acquirer<'_>,
    template: &Args,
    kind_of: &(dyn Fn(&UnmodelledPart) -> String + Sync),
    extract: Extract<'_>,
    console: &Console,
) -> PartRun {
    progress::with_label(&p.part, || {
        let want = Wanted {
            part: p.part.clone(),
            aliases: p.aliases.clone(),
            footprints: p.footprints.clone(),
            description: p.description.clone(),
            manufacturer: p.manufacturer.clone(),
        };
        let sources: Vec<String> = p
            .datasheet
            .iter()
            .chain(&p.more_datasheets)
            .cloned()
            .collect();
        let got = acquirer.acquire(&want, &sources, &mut |line| console.say(&p.part, &line));
        let pdf = match got {
            Outcome::Ready(ready) => ready.pdf,
            Outcome::NoDatasheet(missing) => {
                console.say(&p.part, &words::no_datasheet_found(&p.part, &missing.why));
                return PartRun {
                    ended: Ended::NoDatasheet(missing),
                    needs: Vec::new(),
                };
            }
        };
        let args = Args {
            pdf,
            part: p.part.clone(),
            aliases: p.aliases.clone(),
            footprints: p.footprints.clone(),
            kind_str: kind_of(p),
            ..template.clone()
        }
        .board(p.board.clone());
        let ended = match extract(args) {
            Ok(path) => {
                console.say(&p.part, &format!("{}: drafted {}", p.part, path.display()));
                Ended::Drafted
            }
            Err(e) => {
                let why = format!("{e:#}");
                console.complain(&p.part, &format!("{}: {why}", p.part));
                Ended::Failed(why)
            }
        };
        PartRun {
            ended,
            needs: progress::take_questions(&p.part),
        }
    })
}

/// Get a verified datasheet for each part and draft its card, `opts.jobs` at a
/// time. A part that fails is reported and the batch carries on.
pub fn draft_parts(
    work: &[&UnmodelledPart],
    acquirer: &Acquirer<'_>,
    template: &Args,
    kind_of: &(dyn Fn(&UnmodelledPart) -> String + Sync),
    extract: Extract<'_>,
    opts: &BatchOptions,
) -> Tally {
    let lanes = lanes(work);
    let concurrent = opts.jobs.clamp(1, MAX_JOBS).min(lanes.len().max(1)) > 1;
    let console = Console {
        prefixed: concurrent,
    };
    if concurrent {
        // One ledger for the whole batch, reported once at the end.
        progress::set_concurrent(true);
        hauksbee_models::extract_usage::reset();
    }
    let ran = schedule(
        work.len(),
        lanes,
        opts.jobs,
        opts.progress_every,
        &|at| run_part(work[at], acquirer, template, kind_of, extract, &console),
        &|done, total, running| println!("{}", words::progress(done, total, running)),
    );
    if concurrent {
        progress::set_concurrent(false);
        let cost = hauksbee_models::extract_usage::summary();
        if !cost.is_empty() {
            println!("[model-extract] {cost}");
        }
    }
    let mut tally = Tally::default();
    for (p, run) in work.iter().zip(ran) {
        let Some(run) = run else {
            tally
                .failed
                .push((p.part.clone(), "the extraction crashed".into()));
            continue;
        };
        match run.ended {
            Ended::Drafted => tally.drafted += 1,
            Ended::NoDatasheet(m) => tally.no_datasheet.push((p.part.clone(), m)),
            Ended::Failed(why) => tally.failed.push((p.part.clone(), why)),
        }
        tally
            .needs
            .extend(run.needs.into_iter().map(|question| Need {
                part: p.part.clone(),
                question,
            }));
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn parts_that_would_share_a_file_share_a_lane() {
        let part = |name: &str| UnmodelledPart {
            part: name.into(),
            aliases: vec![],
            footprints: vec![],
            references: vec![],
            datasheet: None,
            more_datasheets: vec![],
            description: None,
            manufacturer: None,
            doubt: None,
            board: None,
        };
        let parts = [part("A/B"), part("C"), part("A_B"), part("a?b")];
        let refs: Vec<&UnmodelledPart> = parts.iter().collect();
        assert_eq!(lanes(&refs), vec![vec![0, 2, 3], vec![1]]);
    }

    /// Eight jobs of 250 ms on four threads take about two rounds, not eight;
    /// never more than four at once; every result lands at its own index.
    #[test]
    fn jobs_run_in_parallel_up_to_the_bound_and_results_keep_their_places() {
        let (in_flight, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let lanes: Vec<Vec<usize>> = (0..8).map(|i| vec![i]).collect();
        let started = Instant::now();
        let out = schedule(
            8,
            lanes,
            4,
            Duration::from_secs(60),
            &|i| {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(250));
                in_flight.fetch_sub(1, Ordering::SeqCst);
                i * 10
            },
            &|_, _, _| {},
        );
        let took = started.elapsed();
        assert_eq!(
            out,
            (0..8).map(|i| Some(i * 10)).collect::<Vec<_>>(),
            "results by index"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert!(
            took < Duration::from_millis(1400),
            "serial would be 2 s; took {took:?}"
        );
    }

    /// One lane is never run in two places at once, and stays in order.
    #[test]
    fn a_lane_runs_in_order_on_one_thread_at_a_time() {
        let busy = AtomicUsize::new(0);
        let order = Mutex::new(Vec::new());
        schedule(
            4,
            vec![vec![0, 1, 2], vec![3]],
            4,
            Duration::from_secs(60),
            &|i| {
                if i < 3 {
                    assert_eq!(busy.fetch_add(1, Ordering::SeqCst), 0, "overlap in a lane");
                    order.lock().unwrap().push(i);
                    std::thread::sleep(Duration::from_millis(30));
                    busy.fetch_sub(1, Ordering::SeqCst);
                }
            },
            &|_, _, _| {},
        );
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
    }

    /// A long quiet stretch says how far it has got: the counts are right and
    /// it speaks again each time the interval passes.
    #[test]
    fn a_quiet_batch_reports_progress() {
        let said = Mutex::new(Vec::new());
        schedule(
            4,
            (0..4).map(|i| vec![i]).collect(),
            2,
            Duration::from_millis(60),
            &|_| std::thread::sleep(Duration::from_millis(300)),
            &|done, total, running| said.lock().unwrap().push((done, total, running)),
        );
        let said = said.into_inner().unwrap();
        assert!(said.len() >= 3, "{said:?}");
        assert_eq!(said[0], (0, 4, 2), "{said:?}");
        assert!(said.iter().all(|s| s.1 == 4 && s.2 <= 2), "{said:?}");
        assert!(said.iter().any(|s| s.0 == 2), "{said:?}");
        assert_eq!(
            words::progress(2, 4, 2),
            "[batch] 2 of 4 parts done, 2 running"
        );
    }

    fn named(name: &str) -> UnmodelledPart {
        UnmodelledPart {
            part: name.into(),
            aliases: vec![],
            footprints: vec![],
            references: vec![],
            datasheet: None,
            more_datasheets: vec![],
            description: None,
            manufacturer: None,
            doubt: None,
            board: None,
        }
    }

    /// Eight parts through the real `draft_parts` with a fake extractor that
    /// takes 250 ms each: four at a time is about a quarter of the serial time,
    /// and the tally keeps the three outcomes apart, in board order.
    #[test]
    fn a_batch_of_eight_with_four_jobs_takes_about_a_quarter_of_the_serial_time() {
        use hauksbee_models::datasheet_discovery::supplied_pdf;
        use hauksbee_models::datasheet_fetch::{CurlHttp, HostLedger};
        let dir = tempfile::tempdir().unwrap();
        // Seven parts have a PDF supplied; P7 has none and no searcher, so it
        // is a part with no datasheet.
        let names: Vec<String> = (0..8).map(|i| format!("P{i}")).collect();
        for n in &names[..7] {
            std::fs::write(dir.path().join(format!("{n}.pdf")), b"%PDF-1.4\n").unwrap();
        }
        let supplied = [dir.path().to_path_buf()];
        assert!(supplied_pdf(&supplied, "P3", &[]).is_some());
        let (http, hosts) = (CurlHttp::default(), HostLedger::default());
        let fetched = dir.path().join("fetched");
        let acquirer = Acquirer {
            http: &http,
            searcher: None,
            dir: &fetched,
            supplied: &supplied,
            hosts: &hosts,
            read_text: |_| String::new(),
        };
        let parts: Vec<UnmodelledPart> = names.iter().map(|n| named(n)).collect();
        let work: Vec<&UnmodelledPart> = parts.iter().collect();
        let template = Args::new(PathBuf::new(), String::new(), String::new());
        let kind_of = |_: &UnmodelledPart| String::from("opamp");
        let (running, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let extract = |args: Args| -> anyhow::Result<PathBuf> {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(250));
            running.fetch_sub(1, Ordering::SeqCst);
            if args.part == "P5" {
                anyhow::bail!("the model refused");
            }
            Ok(PathBuf::from(format!("{}.toml", args.part)))
        };
        let run = |jobs: usize| {
            let opts = BatchOptions {
                jobs,
                progress_every: Duration::from_secs(60),
                ..BatchOptions::default()
            };
            let started = Instant::now();
            let tally = draft_parts(&work, &acquirer, &template, &kind_of, &extract, &opts);
            (tally, started.elapsed())
        };
        let (tally, fast) = run(4);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(tally.drafted, 6);
        assert_eq!(tally.failed.len(), 1);
        assert_eq!(tally.failed[0].0, "P5");
        assert!(tally.failed[0].1.contains("refused"));
        assert_eq!(tally.no_datasheet.len(), 1);
        assert_eq!(tally.no_datasheet[0].0, "P7");
        assert!(
            !progress::concurrent(),
            "the batch leaves the flag as it found it"
        );
        peak.store(0, Ordering::SeqCst);
        let (serial_tally, slow) = run(1);
        assert_eq!(serial_tally.drafted, 6);
        assert_eq!(peak.load(Ordering::SeqCst), 1, "jobs 1 never overlaps");
        assert!(
            fast * 2 < slow,
            "4 jobs {fast:?} should be well under half of serial {slow:?}"
        );
        assert!(slow >= Duration::from_millis(7 * 250), "{slow:?}");
    }

    /// A job that panics is a gap, not the end of the batch.
    #[test]
    fn a_panicking_job_leaves_a_gap_and_the_rest_finish() {
        let out = schedule(
            3,
            (0..3).map(|i| vec![i]).collect(),
            2,
            Duration::from_secs(60),
            &|i| {
                assert_ne!(i, 1, "boom");
                i
            },
            &|_, _, _| {},
        );
        assert_eq!(out, vec![Some(0), None, Some(2)]);
    }
}
