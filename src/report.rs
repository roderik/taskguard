//! Words and screens: the stderr status lines of a job, and the `status`
//! report. Both are rendered from `decide()` output, so they always say what
//! the scheduler really thinks.

use crate::machine::MachineSample;
use crate::queue::{Blocker, Decision, Entry, Limits, reserve};
use serde::Serialize;
use std::fmt::Write as _;

pub fn gb(kb: u64) -> String {
    let mb = kb as f64 / 1024.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{:.0} MB", mb) }
}

pub fn dur(s: f64) -> String {
    let s = s.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub fn say(line: &str) {
    eprintln!("[taskguard] {line}");
}

/// One blocker in words, with the numbers that explain it.
pub fn blocker_text(b: &Blocker) -> String {
    match b {
        Blocker::Older { key } => format!("an older job starts first ({key})"),
        Blocker::Reserved { key, waited_s } => {
            format!("held back so {key}, waiting {}, can start first", dur(*waited_s))
        }
        Blocker::Slots { pool, busy, max, holders } => {
            format!("pool {pool}: {busy} of {max} busy ({})", holders.join(", "))
        }
        Blocker::LearnStagger { wait_s } => {
            format!("new jobs start in small batches while their needs are learned (next batch in {:.1}s)", wait_s)
        }
        Blocker::Settling { key, wait_s } => {
            format!(
                "{key} is a first run that still grows; new first runs wait until it shows what it takes (at most {:.0}s more)",
                wait_s.ceil()
            )
        }
        Blocker::Pressure { level, .. } => {
            format!("memory pressure: the kernel reports {} ({level:.0}); nothing new starts until it eases", pressure_word(*level))
        }
        Blocker::Memory { would_pct, limit_pct, short_kb, .. } => {
            format!("memory: would reach {would_pct:.0}% (limit {limit_pct:.0}%), {} short", gb(*short_kb))
        }
        Blocker::Cpu { would, limit, short, .. } => {
            format!("CPU: would use {would:.1} of {limit:.1} cores, {short:.1} cores short")
        }
    }
}

/// Words for a pressure reading: macOS reports levels (50 warning, 100
/// critical), Linux the share of time tasks waited on memory.
pub fn pressure_word(level: f64) -> &'static str {
    if level >= 100.0 {
        "critical"
    } else if level >= 50.0 {
        "warning"
    } else {
        "some pressure"
    }
}

pub fn main_blocker(d: &Decision) -> Option<&Blocker> {
    match d {
        Decision::Wait { blockers } => blockers.first(),
        Decision::Admit { .. } => None,
    }
}

/// What must happen before a waiting job can start, in one sentence, with an
/// estimate from the learned durations of the running jobs when there is one.
pub fn unblock_text(b: &Blocker, running: &[Entry], now: f64) -> String {
    let remaining = |e: &Entry| e.est_dur_s.map(|d| (d - (now - e.started_at.unwrap_or(now))).max(1.0));
    let first_to_free = |need: &dyn Fn(&Entry) -> f64, short: f64| -> Option<(String, Option<f64>)> {
        let mut rs: Vec<&Entry> = running.iter().collect();
        rs.sort_by(|a, b| remaining(a).unwrap_or(f64::MAX).total_cmp(&remaining(b).unwrap_or(f64::MAX)));
        let mut freed = 0.0;
        for e in rs {
            freed += need(e);
            if freed >= short {
                return Some((e.key.clone(), remaining(e)));
            }
        }
        None
    };
    let when = |key: String, rem: Option<f64>| match rem {
        Some(r) => format!("starts when {key} finishes (about {} left)", dur(r)),
        None => format!("starts when {key} finishes"),
    };
    match b {
        Blocker::Older { key } | Blocker::Reserved { key, .. } => format!("starts after {key} has started"),
        Blocker::Slots { holders, .. } => match holders.first() {
            Some(h) => when(h.clone(), running.iter().find(|e| &e.key == h).and_then(remaining)),
            None => "starts when a slot frees up".into(),
        },
        Blocker::LearnStagger { wait_s } => format!("starts in about {:.0}s", wait_s.ceil()),
        Blocker::Settling { key, wait_s } => format!("starts when {key} stops growing (at most {:.0}s)", wait_s.ceil()),
        Blocker::Pressure { .. } => "starts when the memory pressure eases".into(),
        Blocker::Memory { short_kb, .. } => {
            let need = |e: &Entry| e.need_mem_kb.max(e.live_mem_kb) as f64;
            match first_to_free(&need, *short_kb as f64) {
                Some((k, r)) => when(k, r),
                None => format!("starts when {} of memory frees up", gb(*short_kb)),
            }
        }
        Blocker::Cpu { short, .. } => {
            let need = |e: &Entry| e.need_cpu.max(e.live_cpu);
            match first_to_free(&need, *short) {
                Some((k, r)) => when(k, r),
                None => format!("starts when {short:.1} cores free up"),
            }
        }
    }
}

// ---------------------------------------------------------- status lines ----

/// What the `done` line reports.
#[derive(Clone, Copy)]
pub struct Done<'a> {
    pub key: &'a str,
    pub secs: f64,
    pub used: f64,
    pub wanted: f64,
    pub peak_kb: u64,
    pub code: i32,
    pub queued: usize,
}

pub struct Lines {
    pub hints: bool,
    pub quiet: bool,
}

impl Lines {
    fn out(&self, text: String, hint: &str) {
        if self.hints && !hint.is_empty() {
            say(&format!("{text}. {hint}"));
        } else {
            say(&text);
        }
    }

    pub fn queued(&self, e: &Entry, runs: usize, timeout: Option<f64>) {
        if self.quiet {
            return;
        }
        let what = if e.known || e.raised_by_min {
            let src = if e.raised_by_min { "needs (with minimum)" } else { "learned" };
            let runs = if runs > 0 { format!(" ({runs} runs)") } else { String::new() };
            format!("queued {} - {src}: {:.1} cores, {}{runs}", e.key, e.need_cpu, gb(e.need_mem_kb))
        } else {
            format!(
                "queued {} - first run; reserving an estimate of {:.1} cores, {} ({})",
                e.key,
                e.need_cpu,
                gb(e.need_mem_kb),
                e.estimate_from.as_deref().unwrap_or("a default")
            )
        };
        self.out(format!("{what}{}", timeout_text(timeout)), "Waiting for room is normal; this is not a hang. Inspect: taskguard status");
    }

    pub fn waiting(&self, e: &Entry, waited: f64, d: &Decision, running: &[Entry], now: f64, timeout: Option<f64>) {
        let Some(b) = main_blocker(d) else { return };
        let others: Vec<&str> = match d {
            Decision::Wait { blockers } => blockers.iter().skip(1).map(|b| b.name()).collect(),
            _ => vec![],
        };
        let also = if others.is_empty() { String::new() } else { format!(" (also: {})", others.join(", ")) };
        let text = format!("waiting {} {} - blocked by {}{also}{}", dur(waited), e.key, blocker_text(b), timeout_text(timeout));
        let hint = format!("It {}; this is not a hang", unblock_text(b, running, now));
        self.out(text, &hint);
    }

    pub fn start(&self, e: &Entry, waited: f64, reason: &str) {
        if self.quiet && waited < 1.0 {
            return;
        }
        self.out(format!("start {} - waited {}; {reason}", e.key, dur(waited)), "");
    }

    pub fn now(&self, e: &Entry) {
        if self.quiet {
            return;
        }
        self.out(format!("now {} - queue skipped on request, still measured", e.key), "");
    }

    pub fn done(&self, d: &Done) {
        let Done { key, secs, used, wanted, peak_kb, code, queued } = *d;
        if self.quiet {
            return;
        }
        let load = if peak_kb > 0 {
            format!("used {used:.1} cores sustained (wanted {wanted:.1}), {} peak, ", gb(peak_kb))
        } else {
            String::new()
        };
        self.out(format!("done {key} - {}, {load}exit {code} ({queued} still queued)", dur(secs)), "");
    }
}

fn timeout_text(t: Option<f64>) -> String {
    match t {
        Some(s) if s > 0.0 => format!("; runs anyway in {}", dur(s)),
        Some(s) if s < 0.0 => format!("; gives up in {}", dur(-s)),
        _ => String::new(),
    }
}

// --------------------------------------------------------------- status ----

#[derive(Serialize)]
pub struct Snapshot {
    pub now: f64,
    pub machine: MachineSample,
    #[serde(skip)]
    pub limits: Limits,
    pub cpu_limit: f64,
    pub mem_limit_kb: u64,
    pub reserve_cpu: f64,
    pub reserve_mem_kb: u64,
    pub running: Vec<Entry>,
    pub waiting: Vec<Waiting>,
    pub warnings: Vec<String>,
    pub advice: Vec<String>,
    pub top_other: Vec<(String, u64, f64)>,
}

#[derive(Serialize, Clone)]
pub struct Waiting {
    pub entry: Entry,
    pub decision: Decision,
    pub blocked_by: Option<String>,
    pub next: Option<String>,
}

impl Snapshot {
    pub fn build(
        machine: MachineSample,
        limits: Limits,
        running: Vec<Entry>,
        waiting: Vec<Entry>,
        now: f64,
        unknown_starts: &[f64],
    ) -> Snapshot {
        let (rc, rm) = reserve(&running);
        let w = waiting
            .iter()
            .map(|e| {
                let d = crate::queue::decide(&machine, &limits, &running, &waiting, e, now, unknown_starts);
                let b = main_blocker(&d).cloned();
                Waiting {
                    entry: e.clone(),
                    blocked_by: b.as_ref().map(blocker_text),
                    next: b.as_ref().map(|b| unblock_text(b, &running, now)),
                    decision: d,
                }
            })
            .collect();
        Snapshot {
            now,
            cpu_limit: machine.ncpu as f64 * limits.cpu_max_pct / 100.0,
            mem_limit_kb: (machine.mem_total_kb as f64 * limits.mem_max_pct / 100.0) as u64,
            machine,
            limits,
            reserve_cpu: rc,
            reserve_mem_kb: rm,
            running,
            waiting: w,
            warnings: Vec::new(),
            advice: Vec::new(),
            top_other: Vec::new(),
        }
    }
}

/// Every admission rule for one waiting job, passed or failed, with its
/// numbers: the heart of the dashboard's Why panel.
pub fn rule_checks(s: &Snapshot, w: &Waiting) -> Vec<(bool, String)> {
    let e = &w.entry;
    let m = &s.machine;
    let blockers: Vec<&Blocker> = match &w.decision {
        Decision::Wait { blockers } => blockers.iter().collect(),
        Decision::Admit { .. } => vec![],
    };
    let failed = |name: &str| blockers.iter().find(|b| b.name() == name).copied();
    let mut out = Vec::new();
    if let Some(b) = failed("order").or(failed("reserved")) {
        out.push((false, format!("order: {}", blocker_text(b))));
    } else {
        out.push((true, "order: no older job is held back for this one".into()));
    }
    if let Some(max) = e.pool_slots {
        let busy = s.running.iter().filter(|r| r.pool_key == e.pool_key).count();
        out.push((failed("slots").is_none(), format!("slots: pool {} has {busy} of {max} busy", e.pool.as_deref().unwrap_or("-"))));
    }
    if !e.known && !e.raised_by_min {
        out.push((
            failed("learning").is_none(),
            match failed("learning") {
                Some(b) => format!("learning: {}", blocker_text(b)),
                None => "learning: first run; new jobs start in batches, each after the one before has run a few seconds".into(),
            },
        ));
    }
    // Under memory pressure the decision stops before it checks CPU and
    // memory, so for those two lines the numbers decide what is shown.
    let pressure = failed("pressure");
    if let Some(b) = pressure {
        out.push((false, blocker_text(b)));
    }
    let checked = |name: &str, fits: bool| if pressure.is_some() { fits } else { failed(name).is_none() };
    let cpu_would = m.cpu_busy + s.reserve_cpu + e.need_cpu;
    if let Some(d) = e.est_dur_s.filter(|d| *d < s.limits.cpu_min_duration) {
        out.push((true, format!("CPU: not checked; this job usually takes {d:.1}s, too short to overload the machine")));
    } else {
        out.push((
            checked("cpu", cpu_would <= s.cpu_limit),
            format!(
                "CPU: {:.1} busy + {:.1} promised to running jobs + {:.1} for this job = {:.1} of {:.1} cores",
                m.cpu_busy, s.reserve_cpu, e.need_cpu, cpu_would, s.cpu_limit
            ),
        ));
    }
    let mem_would = m.mem_used_kb + s.reserve_mem_kb + e.need_mem_kb;
    out.push((
        checked("memory", mem_would <= s.mem_limit_kb),
        format!(
            "memory: {} held + {} promised + {} for this job = {:.0}% (limit {:.0}%)",
            gb(m.mem_used_kb),
            gb(s.reserve_mem_kb),
            gb(e.need_mem_kb),
            mem_would as f64 * 100.0 / m.mem_total_kb.max(1) as f64,
            s.limits.mem_max_pct
        ),
    ));
    out
}

/// A bar of `width` cells: `#` in use now, `+` promised to running jobs,
/// `.` free, and `|` at the limit.
/// `#` in use, `!` the part of it that running jobs use above their needs,
/// `+` promised to running jobs, `|` the limit.
pub fn bar(used: f64, over: f64, reserved: f64, total: f64, limit: f64, width: usize) -> String {
    if total <= 0.0 {
        return ".".repeat(width);
    }
    let cell = |v: f64| ((v / total) * width as f64).round().clamp(0.0, width as f64) as usize;
    let u = cell(used);
    let o = cell(used - over.min(used)).min(u);
    let r = cell(used + reserved).max(u);
    // The marker sits in the cell the limit falls into; a limit at 100% has none.
    let l = (((limit / total) * width as f64).floor().max(0.0) as usize).min(width.saturating_sub(1));
    (0..width)
        .map(|i| {
            if i == l && l + 1 < width {
                '|'
            } else if i < o {
                '#'
            } else if i < u {
                '!'
            } else if i < r {
                '+'
            } else {
                '.'
            }
        })
        .collect()
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

pub fn render_status(s: &Snapshot) -> String {
    let m = &s.machine;
    let mut o = String::new();
    let clock = chrono_like(s.now);
    let _ = writeln!(o, "taskguard   {clock}                       machine: {} cores, {}", m.ncpu, gb(m.mem_total_kb));
    let _ = writeln!(o);
    let (over_cpu, over_mem) = crate::queue::over(&s.running);
    let _ = writeln!(
        o,
        "CPU     [{}]  {:>5.1} busy  +{:.1} reserved  of {:.1} cores   limit {:.0}%{}",
        bar(m.cpu_busy, over_cpu, s.reserve_cpu, m.ncpu as f64, s.cpu_limit, 20),
        m.cpu_busy,
        s.reserve_cpu,
        m.ncpu as f64,
        s.limits.cpu_max_pct,
        if over_cpu >= 0.05 { format!("   {over_cpu:.1} over estimates") } else { String::new() }
    );
    let _ = writeln!(
        o,
        "MEMORY  [{}]  {:>8} held  +{} reserved  of {}   limit {:.0}%{}",
        bar(m.mem_used_kb as f64, over_mem as f64, s.reserve_mem_kb as f64, m.mem_total_kb as f64, s.mem_limit_kb as f64, 20),
        gb(m.mem_used_kb),
        gb(s.reserve_mem_kb),
        gb(m.mem_total_kb),
        s.limits.mem_max_pct,
        if over_mem >= 1024 { format!("   {} over estimates", gb(over_mem)) } else { String::new() }
    );
    let _ = writeln!(o, "         # in use now   ! above the estimates   + promised to running jobs   | limit");
    let _ = writeln!(o);
    let _ =
        writeln!(o, "RUNNING ({})                          pool          time   CPU now / needs      MEMORY now / needs", s.running.len());
    if s.running.is_empty() {
        let _ = writeln!(o, "  (nothing running)");
    }
    for e in &s.running {
        let tag = if e.now { " NOW" } else { "" };
        let est = if e.known || e.raised_by_min { "" } else { " est" };
        let needs_cpu = format!("{:.1}{est}", e.need_cpu);
        let needs_mem = format!("{}{est}", gb(e.need_mem_kb));
        let min = if e.raised_by_min { " min" } else { "" };
        let _ = writeln!(
            o,
            "  {:<36} {:<12} {:>6}   {:>4.1} / {:<9}  {:>8} / {}{min}{tag}",
            trunc(&e.key, 36),
            trunc(e.pool.as_deref().unwrap_or("-"), 12),
            dur(s.now - e.started_at.unwrap_or(s.now)),
            e.live_cpu,
            needs_cpu,
            gb(e.live_mem_kb),
            needs_mem
        );
    }
    let _ = writeln!(o);
    let _ = writeln!(o, "WAITING ({})                          pool        waited   needs                 blocked by", s.waiting.len());
    if s.waiting.is_empty() {
        let _ = writeln!(o, "  (queue empty)");
    }
    for w in &s.waiting {
        let e = &w.entry;
        let est = if e.known || e.raised_by_min { "" } else { " (est)" };
        let needs = format!("{:.1} cores, {}{est}", e.need_cpu, gb(e.need_mem_kb));
        let blocked = match main_blocker(&w.decision) {
            Some(b) => blocker_text(b),
            None => "nothing - starting now".into(),
        };
        let _ = writeln!(
            o,
            "  {:<36} {:<12} {:>6}   {:<21} {}",
            trunc(&e.key, 36),
            trunc(e.pool.as_deref().unwrap_or("-"), 12),
            dur(s.now - e.queued_at),
            needs,
            blocked
        );
    }
    if let Some(w) = s.waiting.iter().find(|w| w.next.is_some()) {
        let _ = writeln!(o);
        let _ = writeln!(o, "NEXT  {} {}.", w.entry.key, w.next.as_deref().unwrap_or(""));
    }
    if let Some(first) = s.warnings.first() {
        let _ = writeln!(o);
        let _ = writeln!(o, "WARNINGS ({})  {first}   (all of them: taskguard top, view 5)", s.warnings.len());
    }
    o
}

/// HH:MM:SS in local time, without pulling in a date crate.
pub fn chrono_like(ts: f64) -> String {
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// YYYY-MM-DD HH:MM in local time.
pub fn datetime(ts: f64) -> String {
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024;

    #[test]
    fn bars() {
        assert_eq!(bar(5.0, 0.0, 2.0, 10.0, 8.5, 10), "#####++.|.");
        assert_eq!(bar(0.0, 0.0, 0.0, 10.0, 10.0, 10), ".........|".replace('|', "."));
        assert_eq!(bar(6.0, 2.0, 1.0, 10.0, 8.5, 10), "####!!+.|.", "2 of the 6 in use are above the estimates");
    }

    #[test]
    fn under_memory_pressure_the_rules_name_it_and_check_the_numbers() {
        let lim = Limits {
            cpu_max_pct: 100.0,
            mem_max_pct: 85.0,
            learn_stagger: 0.0,
            max_bypass: 120.0,
            max_backfill: 0.0,
            cpu_min_duration: 5.0,
            pressure_max: 20.0,
        };
        let mut m = MachineSample::fixed(10.0, 10, 23 * GB, 32 * GB);
        m.mem_pressure = Some(50.0);
        let job = Entry { ticket: 1, key: "root:ci".into(), need_cpu: 10.0, need_mem_kb: GB / 2, known: true, ..Default::default() };
        let s = Snapshot::build(m, lim, vec![], vec![job], 1000.0, &[]);
        let checks = rule_checks(&s, &s.waiting[0]);
        let line = |start: &str| checks.iter().find(|(_, t)| t.starts_with(start)).cloned().unwrap();
        assert!(!line("memory pressure").0, "{checks:?}");
        assert!(!line("CPU").0, "20 of 10 cores does not fit, though the decision stopped before it checked CPU: {checks:?}");
        assert!(line("memory:").0, "{checks:?}");
    }

    #[test]
    fn status_screen_explains_the_blocker() {
        let lim = Limits {
            cpu_max_pct: 100.0,
            mem_max_pct: 85.0,
            learn_stagger: 2.0,
            max_bypass: 120.0,
            max_backfill: 0.0,
            cpu_min_duration: 5.0,
            pressure_max: 20.0,
        };
        let m = MachineSample::fixed(7.4, 12, 23 * GB, 32 * GB);
        let api = Entry {
            ticket: 1,
            key: "packages/api:tsc".into(),
            pool: Some("default".into()),
            need_cpu: 4.0,
            need_mem_kb: 18 * GB,
            live_cpu: 3.1,
            live_mem_kb: 15 * GB,
            known: true,
            started_at: Some(953.0),
            est_dur_s: Some(90.0),
            ..Default::default()
        };
        let worker = Entry {
            ticket: 2,
            key: "packages/worker:tsc".into(),
            need_cpu: 2.0,
            need_mem_kb: 3 * GB,
            known: true,
            queued_at: 969.0,
            ..Default::default()
        };
        let s = Snapshot::build(m, lim, vec![api], vec![worker], 1000.0, &[]);
        let text = render_status(&s);
        assert!(text.contains("memory: would reach 91% (limit 85%), 1.8 GB short"), "{text}");
        assert!(text.contains("NEXT  packages/worker:tsc starts when packages/api:tsc finishes (about 43s left)"), "{text}");
        assert!(text.contains("packages/api:tsc"));
    }
}
