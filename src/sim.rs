//! A small machine simulator for the admission rules, run as tests.
//!
//! taskguard shares the machine with programs it did not start: a browser, an
//! editor, another pipeline that compiles the same repository. It cannot stop
//! their load. What it controls is its own: never start a job while the
//! machine is under pressure, never let its own jobs fill the machine, and
//! still run every job once the other load leaves. Each scenario drives
//! `decide()` exactly as the waiting runners do, tick by tick, and checks
//! those three things.

use crate::machine::{MachineSample, PEAK_WINDOW};
use crate::queue::{Decision, Entry, Limits, decide};

const GB: u64 = 1024 * 1024;
const TOTAL: u64 = 32 * GB;
const NCPU: usize = 10;
const TICK: f64 = 0.5;

/// One job: how much memory and CPU it really takes, for how long, and what
/// taskguard believes it needs before it starts.
#[derive(Clone)]
struct Job {
    peak_kb: u64,
    cores: f64,
    secs: f64,
    known: bool,
    need_kb: u64,
    need_cpu: f64,
    est_dur: Option<f64>,
    /// When it joins the queue.
    arrive: f64,
}

struct Scenario {
    name: &'static str,
    jobs: Vec<Job>,
    /// Memory and CPU of programs outside taskguard, over time.
    other_mem: Box<dyn Fn(f64) -> u64>,
    other_cpu: Box<dyn Fn(f64) -> f64>,
}

#[derive(Debug, Default)]
struct Outcome {
    /// Highest share of memory in use, all programs together.
    worst_pct: f64,
    /// Highest share of memory held by taskguard's own jobs.
    ours_worst_pct: f64,
    /// Jobs started while the kernel reported pressure and others were running.
    started_under_pressure: usize,
    /// Seconds until the last job ended.
    finished_at: f64,
    all_ran: bool,
    /// When each job started, by its place in the scenario.
    started: Vec<Option<f64>>,
}

/// The kernel's pressure level as a function of memory in use, like macOS:
/// warning above 80%, critical above 90%. Memory above 100% is compressed and
/// swapped, which is what froze the machine.
fn pressure(pct: f64) -> f64 {
    if pct > 90.0 {
        100.0
    } else if pct > 80.0 {
        50.0
    } else {
        0.0
    }
}

fn run(s: &Scenario, lim: &Limits) -> Outcome {
    let mut arriving: Vec<Entry> = s
        .jobs
        .iter()
        .enumerate()
        .map(|(i, j)| Entry {
            ticket: i as u64 + 1,
            key: format!("job{i}"),
            need_mem_kb: j.need_kb,
            need_cpu: j.need_cpu,
            known: j.known,
            est_dur_s: j.est_dur,
            queued_at: j.arrive,
            ..Default::default()
        })
        .collect();
    let mut waiting: Vec<Entry> = Vec::new();
    let mut running: Vec<(Entry, f64, Job)> = Vec::new();
    let mut starts: Vec<f64> = Vec::new();
    let mut recent: Vec<(f64, u64, u64)> = Vec::new();
    let mut out = Outcome { started: vec![None; s.jobs.len()], ..Default::default() };
    let mut t = 0.0;
    while t < 3600.0 && (!arriving.is_empty() || !waiting.is_empty() || !running.is_empty()) {
        // Tickets follow arrival, so the queue stays in ticket order.
        let (now_in, later): (Vec<Entry>, Vec<Entry>) = arriving.into_iter().partition(|e| e.queued_at <= t);
        waiting.extend(now_in);
        arriving = later;
        running.retain(|(_, start, j)| {
            let alive = t - start < j.secs;
            if !alive {
                out.finished_at = t;
            }
            alive
        });
        // A job's memory grows over its first 8 seconds, its CPU is immediate.
        for (e, start, j) in running.iter_mut() {
            e.live_mem_kb = (j.peak_kb as f64 * ((t - *start) / 8.0).min(1.0)) as u64;
            e.live_cpu = j.cores;
        }
        let ours: u64 = running.iter().map(|(e, _, _)| e.live_mem_kb).sum();
        let used = (s.other_mem)(t) + ours;
        let pct = used as f64 * 100.0 / TOTAL as f64;
        out.worst_pct = out.worst_pct.max(pct);
        out.ours_worst_pct = out.ours_worst_pct.max(ours as f64 * 100.0 / TOTAL as f64);
        recent.retain(|(ts, _, _)| t - ts < PEAK_WINDOW);
        recent.push((t, used, ours));
        let busy = ((s.other_cpu)(t) + running.iter().map(|(e, _, _)| e.live_cpu).sum::<f64>()).min(NCPU as f64);
        let m = MachineSample {
            ts: t,
            cpu_busy: busy,
            ncpu: NCPU,
            mem_used_kb: used,
            mem_total_kb: TOTAL,
            mem_pressure: Some(pressure(pct)),
            recent_mem: recent.clone(),
            ..Default::default()
        };
        // Every waiting job checks once per tick, oldest first, as the runners do.
        let mut i = 0;
        while i < waiting.len() {
            let run_now: Vec<Entry> = running.iter().map(|(e, _, _)| e.clone()).collect();
            let me = waiting[i].clone();
            if matches!(decide(&m, lim, &run_now, &waiting, &me, t, &starts), Decision::Admit { .. }) {
                if pressure(pct) >= lim.pressure_max && !run_now.is_empty() {
                    out.started_under_pressure += 1;
                }
                if !me.known {
                    starts.push(t);
                }
                // Every older job that still waits has now been passed.
                for w in waiting.iter_mut().filter(|w| w.ticket < me.ticket && w.bypassed_since.is_none()) {
                    w.bypassed_since = Some(t);
                }
                out.started[me.ticket as usize - 1] = Some(t);
                let job = s.jobs[me.ticket as usize - 1].clone();
                running.push((me, t, job));
                waiting.remove(i);
            } else {
                i += 1;
            }
        }
        t += TICK;
    }
    out.all_ran = arriving.is_empty() && waiting.is_empty() && running.is_empty();
    out
}

fn new_tsc(peak_mb: u64) -> Job {
    // A first run: taskguard reserves its 1.5 GB estimate.
    Job { peak_kb: peak_mb * 1024, cores: 1.5, secs: 30.0, known: false, need_kb: 1536 * 1024, need_cpu: 1.0, est_dur: None, arrive: 0.0 }
}

fn learned_tsc(peak_mb: u64) -> Job {
    Job {
        peak_kb: peak_mb * 1024,
        cores: 1.5,
        secs: 30.0,
        known: true,
        need_kb: peak_mb * 1024,
        need_cpu: 1.5,
        est_dur: Some(30.0),
        arrive: 0.0,
    }
}

fn lint() -> Job {
    Job { peak_kb: 200 * 1024, cores: 2.0, secs: 0.5, known: true, need_kb: 200 * 1024, need_cpu: 2.0, est_dur: Some(0.5), arrive: 0.0 }
}

fn mixed_first_runs() -> Vec<Job> {
    (0..60).map(|i| new_tsc([300, 700, 4200, 500, 1200, 900][i % 6])).collect()
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "freeze replay: 60 first runs, 16 GB held by others",
            jobs: mixed_first_runs(),
            other_mem: Box::new(|_| 16 * GB),
            other_cpu: Box::new(|_| 2.0),
        },
        Scenario {
            name: "another pipeline adds 10 GB at 20 s, for 60 s",
            jobs: mixed_first_runs(),
            other_mem: Box::new(|t| 14 * GB + if (20.0..80.0).contains(&t) { 10 * GB } else { 0 }),
            other_cpu: Box::new(|t| if (20.0..80.0).contains(&t) { 6.0 } else { 1.0 }),
        },
        Scenario {
            name: "busy machine: others hold 24 GB (75%) throughout",
            jobs: (0..30).map(|i| learned_tsc([600, 1500, 900][i % 3])).collect(),
            other_mem: Box::new(|_| 24 * GB),
            other_cpu: Box::new(|_| 3.0),
        },
        Scenario {
            name: "browser swings 4 GB up and down every 6 s",
            jobs: (0..30).map(|i| learned_tsc([600, 1500, 2500][i % 3])).collect(),
            other_mem: Box::new(|t| 18 * GB + if ((t / 6.0) as u64).is_multiple_of(2) { 4 * GB } else { 0 }),
            other_cpu: Box::new(|_| 2.0),
        },
        Scenario {
            name: "others use every core; 40 lints and 10 compiles",
            jobs: (0..40).map(|_| lint()).chain((0..10).map(|_| learned_tsc(800))).collect(),
            other_mem: Box::new(|_| 12 * GB),
            other_cpu: Box::new(|_| 10.0),
        },
        Scenario {
            name: "others hold the machine at critical pressure for 5 min",
            jobs: (0..20).map(|_| learned_tsc(700)).collect(),
            other_mem: Box::new(|t| if t < 300.0 { 30 * GB } else { 12 * GB }),
            other_cpu: Box::new(|_| 2.0),
        },
    ]
}

/// A job with a known need that arrives at `arrive`.
fn known(mem_gb: u64, cores: f64, secs: f64, arrive: f64) -> Job {
    Job { peak_kb: mem_gb * GB, cores, secs, known: true, need_kb: mem_gb * GB, need_cpu: cores, est_dur: Some(secs), arrive }
}

/// A 12 GB job queues behind a 14 GB compile that runs for 10 minutes and
/// cannot start next to it. Small jobs arrive every 20 s; each fits easily.
fn head_of_line() -> Scenario {
    let mut jobs = vec![known(14, 2.0, 600.0, 0.0), known(12, 2.0, 300.0, 1.0)];
    jobs.extend((0..30).map(|i| known(1, 0.5, 30.0, 2.0 + 20.0 * i as f64)));
    Scenario {
        name: "a 12 GB job waits for a compile; small jobs arrive every 20 s",
        jobs,
        other_mem: Box::new(|_| 4 * GB),
        other_cpu: Box::new(|_| 1.0),
    }
}

/// Small jobs arrive every 2 s for 25 minutes, enough to keep memory at the
/// limit on their own. A 12 GB job arrives after a minute and never finds
/// 12 GB free while they keep coming.
fn steady_stream() -> Scenario {
    let mut jobs: Vec<Job> = (0..30).map(|i| known(1, 0.2, 60.0, 2.0 * i as f64)).collect();
    jobs.push(known(12, 2.0, 60.0, 60.0));
    jobs.extend((30..750).map(|i| known(1, 0.2, 60.0, 2.0 * i as f64)));
    Scenario { name: "small jobs every 2 s keep memory full", jobs, other_mem: Box::new(|_| 4 * GB), other_cpu: Box::new(|_| 1.0) }
}

/// The longest any small job (all but the scenario's large ones) waited.
fn worst_small_wait(s: &Scenario, o: &Outcome) -> f64 {
    s.jobs
        .iter()
        .zip(&o.started)
        .filter(|(j, _)| j.peak_kb <= GB)
        .map(|(j, t)| t.expect("every job started") - j.arrive)
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIM: Limits = Limits {
        cpu_max_pct: 100.0,
        mem_max_pct: 85.0,
        learn_stagger: 5.0,
        max_bypass: 120.0,
        max_backfill: 0.0,
        cpu_min_duration: 5.0,
        pressure_max: 20.0,
    };

    #[test]
    fn taskguard_holds_back_when_other_programs_load_the_machine() {
        eprintln!("{:<56} {:>7} {:>7} {:>9} {:>8}", "scenario", "worst", "ours", "started", "done");
        eprintln!("{:<56} {:>7} {:>7} {:>9} {:>8}", "", "memory", "memory", "under", "after");
        eprintln!("{:<56} {:>7} {:>7} {:>9} {:>8}", "", "", "", "pressure", "");
        for s in scenarios() {
            let o = run(&s, &LIM);
            eprintln!(
                "{:<56} {:>6.0}% {:>6.0}% {:>9} {:>7.0}s",
                s.name, o.worst_pct, o.ours_worst_pct, o.started_under_pressure, o.finished_at
            );
            assert!(o.all_ran, "{}: every job still runs", s.name);
            assert_eq!(o.started_under_pressure, 0, "{}: nothing starts under pressure", s.name);
            // Other programs may push the machine up; taskguard's own jobs
            // must stay well inside the limit on top of what they found.
            assert!(o.ours_worst_pct < 85.0, "{}: our jobs stay inside the limit ({:.0}%)", s.name, o.ours_worst_pct);
            assert!(o.worst_pct < 100.0 || s.name.starts_with("others hold"), "{}: memory never overflows ({:.0}%)", s.name, o.worst_pct);
        }
    }

    #[test]
    fn the_old_rules_overflowed_in_the_same_scenarios() {
        let old = Limits { learn_stagger: 2.0, pressure_max: f64::MAX, ..LIM };
        let freeze = &scenarios()[0];
        let mut jobs = freeze.jobs.clone();
        for j in &mut jobs {
            // A first run counted as needing nothing, memory or CPU.
            j.need_kb = 0;
            j.need_cpu = 0.0;
        }
        let s = Scenario { name: freeze.name, jobs, other_mem: Box::new(|_| 16 * GB), other_cpu: Box::new(|_| 2.0) };
        let o = run(&s, &old);
        eprintln!("old rules, freeze replay: worst memory {:.0}%", o.worst_pct);
        assert!(o.worst_pct > 95.0, "the old rules fill the machine, as in the real freeze (97%): {:.0}%", o.worst_pct);
    }

    #[test]
    fn small_jobs_start_while_a_reserved_job_waits_for_memory() {
        let s = head_of_line();
        let strict = run(&s, &LIM);
        let backfill = run(&s, &Limits { max_backfill: 1800.0, ..LIM });
        let (head_strict, head_backfill) = (strict.started[1].unwrap(), backfill.started[1].unwrap());
        let (wait_strict, wait_backfill) = (worst_small_wait(&s, &strict), worst_small_wait(&s, &backfill));
        eprintln!("{}: strict: large job at {head_strict:.0}s, small jobs wait up to {wait_strict:.0}s", s.name);
        eprintln!("{}: backfill: large job at {head_backfill:.0}s, small jobs wait up to {wait_backfill:.0}s", s.name);
        assert!(strict.all_ran && backfill.all_ran);
        assert!(wait_strict > 300.0, "the reservation holds small jobs back for minutes: {wait_strict:.0}s");
        assert!(wait_backfill < 5.0, "small jobs start as they arrive: {wait_backfill:.0}s");
        assert!(head_backfill <= head_strict + 1.0, "the large job starts no later: {head_backfill:.0}s vs {head_strict:.0}s");
    }

    #[test]
    fn max_backfill_bounds_the_wait_of_a_job_small_jobs_keep_out() {
        let s = steady_stream();
        let head = |lim: &Limits| {
            let o = run(&s, lim);
            assert!(o.all_ran && o.started_under_pressure == 0, "{}", s.name);
            o.started[30].unwrap() - 60.0
        };
        let strict = head(&LIM);
        let unbounded = head(&Limits { max_backfill: f64::MAX, ..LIM });
        let bounded = head(&Limits { max_backfill: 600.0, ..LIM });
        eprintln!("{}: the 12 GB job waits {strict:.0}s strict, {unbounded:.0}s unbounded, {bounded:.0}s with max_backfill 600", s.name);
        // Without a bound the large job starts only when the stream ends.
        assert!(unbounded > 1400.0, "{unbounded:.0}s");
        // With one, newer jobs stop at 600 s and it starts once they drain.
        assert!((600.0..600.0 + 90.0).contains(&bounded), "{bounded:.0}s");
        assert!(strict < 120.0 + 90.0, "{strict:.0}s");
    }
}
