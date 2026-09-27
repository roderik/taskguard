//! The machine-wide queue: one file per waiting job and one per running job,
//! under a state directory, guarded by one file lock.
//!
//! The owner pid is part of every file name, so a job that dies (Ctrl-C,
//! SIGKILL) is reaped by the next process that looks, and never wedges the
//! queue. The kernel releases the lock itself when its holder dies, which
//! replaces tsc-queue's mkdir lock and its stale-age guess.

use crate::machine::MachineSample;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

/// One waiting or running job, as a JSON file in `wait/` or `run/`.
///
/// Every taskguard version on the machine reads these files, and DALP pins a
/// version per worktree, so old and new versions run side by side. The rules:
/// - Never remove or rename a field, and never change its JSON type. v0.1.0 to
///   v0.1.2 need every field below except `estimate_from`; an entry they cannot
///   read is a job they do not see, and they start work on top of it.
/// - A new field is optional: `#[serde(default)]` covers it, so this version
///   still reads the entries of older ones.
/// - A change that cannot follow these rules needs a new state directory.
///
/// `tests::entry_format_is_stable` holds the v0.1.2 shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Entry {
    pub ticket: u64,
    pub pid: i32,
    pub run_id: i64,
    pub key: String,
    pub ns: String,
    pub label: Option<String>,
    /// The pool name as shown, and the pool identity used for counting slots
    /// (it includes the checkout for per-checkout pools).
    pub pool: Option<String>,
    pub pool_key: Option<String>,
    pub pool_slots: Option<u32>,
    pub checkout: String,
    pub queued_at: f64,
    pub started_at: Option<f64>,
    pub need_cpu: f64,
    pub need_mem_kb: u64,
    /// False while the job has no history: its needs are still unknown.
    pub known: bool,
    /// A minimum raised the need above what was learned.
    pub raised_by_min: bool,
    /// Started with --now: it skipped the queue.
    pub now: bool,
    pub live_cpu: f64,
    pub live_mem_kb: u64,
    /// When a newer job first started while this one waited.
    pub bypassed_since: Option<f64>,
    /// The main blocker, in words, for the status screen, and since when it holds.
    pub blocker: Option<String>,
    pub blocker_since: Option<f64>,
    /// Learned median duration, for estimated start times.
    pub est_dur_s: Option<f64>,
    /// For a job with no history: where its estimated need comes from.
    pub estimate_from: Option<String>,
    /// The taskguard version that owns the entry. Older versions leave it
    /// out, and they do not read nudges either.
    pub version: Option<String>,
    /// Set when someone changed the needs by hand, in words.
    pub by_hand: Option<String>,
    /// When the job's memory last rose more than 10% above its peak so far.
    /// A first run that has stopped growing shows what it takes.
    pub mem_grew_at: Option<f64>,
    /// The needs the job started with. While it runs, its needs follow its
    /// peak; these keep what it was admitted with, to show the overage.
    pub start_need_cpu: Option<f64>,
    pub start_need_mem_kb: Option<u64>,
}

/// A first run has settled when its memory has not grown for this long...
pub const SETTLE_S: f64 = 20.0;
/// ...or when it has run this long: a long job must not hold back every new
/// one, and by then its needs follow its peak.
pub const SETTLE_MAX_S: f64 = 120.0;

impl Entry {
    /// Still on a guess: no history, no minimum, no needs set by hand.
    pub fn guessed(&self) -> bool {
        !self.known && !self.raised_by_min && self.by_hand.is_none()
    }

    /// How long until this running first run counts as settled; None once it has.
    pub fn settling_for(&self, now: f64) -> Option<f64> {
        let started = self.started_at?;
        if !self.guessed() {
            return None;
        }
        let grew = self.mem_grew_at.unwrap_or(started);
        let left = (SETTLE_S - (now - grew)).min(SETTLE_MAX_S - (now - started));
        (left > 0.0).then_some(left)
    }
}

/// A request from the dashboard to one job: start now, or use other needs.
/// The job's own process reads it (`nudge/<pid>`) and acts on it, because
/// only that process may move its entry or decide that it starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Nudge {
    pub start: bool,
    pub need_cpu: Option<f64>,
    pub need_mem_kb: Option<u64>,
}

pub struct Queue {
    pub dir: PathBuf,
}

pub struct Guard {
    _file: File,
}

impl Queue {
    pub fn open(dir: &Path) -> Result<Queue> {
        for d in ["wait", "run", "viewers", "nudge"] {
            fs::create_dir_all(dir.join(d)).with_context(|| format!("creating {}", dir.join(d).display()))?;
        }
        Ok(Queue { dir: dir.to_path_buf() })
    }

    pub fn lock(&self) -> Result<Guard> {
        let f = OpenOptions::new().create(true).truncate(false).write(true).open(self.dir.join("lock"))?;
        f.lock()?;
        Ok(Guard { _file: f })
    }

    pub fn next_ticket(&self) -> Result<u64> {
        let p = self.dir.join("seq");
        let n: u64 = fs::read_to_string(&p).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0) + 1;
        fs::write(&p, n.to_string())?;
        Ok(n)
    }

    pub fn wait_path(&self, e: &Entry) -> PathBuf {
        self.dir.join("wait").join(format!("{}.{}", e.ticket, e.pid))
    }

    pub fn run_path(&self, pid: i32) -> PathBuf {
        self.dir.join("run").join(pid.to_string())
    }

    fn nudge_path(&self, pid: i32) -> PathBuf {
        self.dir.join("nudge").join(pid.to_string())
    }

    /// Add to the nudge for a job; an earlier one that was not read yet stays.
    pub fn nudge(&self, pid: i32, change: impl FnOnce(&mut Nudge)) -> Result<()> {
        let path = self.nudge_path(pid);
        let mut n: Nudge = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        change(&mut n);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, serde_json::to_string(&n)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Read and remove the nudge for a job, if there is one.
    pub fn take_nudge(&self, pid: i32) -> Option<Nudge> {
        let path = self.nudge_path(pid);
        let text = fs::read_to_string(&path).ok()?;
        let _ = fs::remove_file(&path);
        serde_json::from_str(&text).ok()
    }

    /// Write through a temp file and rename, so a reader never sees half a file.
    pub fn write(&self, path: &Path, e: &Entry) -> Result<()> {
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, serde_json::to_string(e)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn read_dir(&self, sub: &str) -> Vec<(PathBuf, Entry)> {
        let Ok(rd) = fs::read_dir(self.dir.join(sub)) else {
            return Vec::new();
        };
        let mut v: Vec<(PathBuf, Entry)> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            // Skip half-written files. Only the file name is checked: the state
            // directory itself may sit under a path that contains ".tmp".
            .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().contains(".tmp")))
            .filter_map(|p| {
                let e: Entry = serde_json::from_str(&fs::read_to_string(&p).ok()?).ok()?;
                Some((p, e))
            })
            .collect();
        v.sort_by_key(|(_, e)| e.ticket);
        v
    }

    pub fn waiting(&self) -> Vec<Entry> {
        self.read_dir("wait").into_iter().map(|(_, e)| e).collect()
    }

    pub fn running(&self) -> Vec<Entry> {
        self.read_dir("run").into_iter().map(|(_, e)| e).collect()
    }

    /// Drop entries whose owner process is gone. Returns the run ids that
    /// were abandoned, so the caller can close them in the database.
    pub fn reap(&self) -> Vec<i64> {
        let mut gone = Vec::new();
        for sub in ["wait", "run"] {
            for (p, e) in self.read_dir(sub) {
                if !alive(e.pid) {
                    let _ = fs::remove_file(&p);
                    gone.push(e.run_id);
                }
            }
            // An entry this version cannot read (a newer version broke the
            // format rules on `Entry`) still names its owner in the file name:
            // `wait/<ticket>.<pid>` and `run/<pid>`. Drop it once that is gone.
            let Ok(rd) = fs::read_dir(self.dir.join(sub)) else { continue };
            for p in rd.filter_map(|e| e.ok()).map(|e| e.path()) {
                let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
                let owner = name.rsplit('.').next().and_then(|pid| pid.parse::<i32>().ok());
                if name.contains(".tmp") || fs::read_to_string(&p).is_ok_and(|t| serde_json::from_str::<Entry>(&t).is_ok()) {
                    continue;
                }
                if owner.is_some_and(|pid| !alive(pid)) {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        for sub in ["viewers", "nudge"] {
            let Ok(rd) = fs::read_dir(self.dir.join(sub)) else { continue };
            for f in rd.filter_map(|e| e.ok()) {
                let pid: i32 = f.file_name().to_string_lossy().split('.').next().and_then(|p| p.parse().ok()).unwrap_or(0);
                if !alive(pid) {
                    let _ = fs::remove_file(f.path());
                }
            }
        }
        gone
    }

    pub fn viewers(&self) -> usize {
        fs::read_dir(self.dir.join("viewers")).map(|rd| rd.count()).unwrap_or(0)
    }

    /// When recent jobs with no history started, newest last.
    pub fn unknown_starts(&self) -> Vec<f64> {
        fs::read_to_string(self.dir.join("unknown_starts"))
            .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Record a start and forget the ones older than `window` seconds.
    pub fn add_unknown_start(&self, t: f64, window: f64) {
        let mut v: Vec<f64> = self.unknown_starts().into_iter().filter(|s| t - s < window).collect();
        v.push(t);
        let text: String = v.iter().map(|s| format!("{s}\n")).collect();
        let _ = fs::write(self.dir.join("unknown_starts"), text);
    }

    /// When the queue was last busy, for the recorder's idle exit.
    pub fn touch_activity(&self) {
        let _ = fs::write(self.dir.join("last_activity"), crate::db::now().to_string());
    }

    pub fn last_activity(&self) -> f64 {
        fs::read_to_string(self.dir.join("last_activity")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0)
    }
}

pub fn alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0) checks existence without sending anything. EPERM means the
    // process exists but belongs to someone else.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// ---------------------------------------------------------------- decide ----

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub cpu_max_pct: f64,
    pub mem_max_pct: f64,
    pub learn_stagger: f64,
    pub max_bypass: f64,
    /// Until an older job has waited this long, its reservation holds only
    /// while it could start; newer jobs that fit may start while it cannot.
    /// At or below `max_bypass` (0 by default) a reservation always holds.
    pub max_backfill: f64,
    /// Jobs that usually end sooner than this skip the CPU check.
    pub cpu_min_duration: f64,
    /// Memory pressure (0-100) at which nothing new starts.
    pub pressure_max: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Blocker {
    /// Nothing runs, but an older job is first in line.
    Older {
        key: String,
    },
    /// An older job has been passed for too long; it goes first now.
    Reserved {
        key: String,
        waited_s: f64,
    },
    Slots {
        pool: String,
        busy: u32,
        max: u32,
        holders: Vec<String>,
    },
    LearnStagger {
        wait_s: f64,
    },
    /// A first run still grows: another first run waits until it has shown
    /// what it takes.
    Settling {
        key: String,
        wait_s: f64,
    },
    /// The kernel reports memory pressure: nothing new starts.
    Pressure {
        level: f64,
        limit: f64,
    },
    Memory {
        would_pct: f64,
        limit_pct: f64,
        short_kb: u64,
        used_kb: u64,
        reserve_kb: u64,
        need_kb: u64,
        total_kb: u64,
    },
    Cpu {
        would: f64,
        limit: f64,
        short: f64,
        busy: f64,
        reserve: f64,
        need: f64,
    },
}

impl Blocker {
    pub fn name(&self) -> &'static str {
        match self {
            Blocker::Older { .. } => "order",
            Blocker::Reserved { .. } => "reserved",
            Blocker::Slots { .. } => "slots",
            Blocker::LearnStagger { .. } | Blocker::Settling { .. } => "learning",
            Blocker::Memory { .. } => "memory",
            Blocker::Pressure { .. } => "pressure",
            Blocker::Cpu { .. } => "cpu",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Admit { reason: String },
    Wait { blockers: Vec<Blocker> },
}

/// Memory that running jobs have not claimed yet: a job sitting at 2 GB whose
/// history says it peaks at 20 GB still has 18 GB to take, and that has to be
/// reserved now. Without this, a second job is admitted into space the first
/// one is about to eat. The same holds for CPU.
pub fn reserve(running: &[Entry]) -> (f64, u64) {
    running.iter().fold((0.0, 0), |(c, m), e| ((c + (e.need_cpu - e.live_cpu).max(0.0)), m + e.need_mem_kb.saturating_sub(e.live_mem_kb)))
}

/// How far running jobs are above their needs right now: the part of the
/// load the estimates did not see coming.
pub fn over(running: &[Entry]) -> (f64, u64) {
    running.iter().fold((0.0, 0), |(c, m), e| {
        let (cpu, mem) = (e.start_need_cpu.unwrap_or(e.need_cpu), e.start_need_mem_kb.unwrap_or(e.need_mem_kb));
        (c + (e.live_cpu - cpu).max(0.0), m + e.live_mem_kb.saturating_sub(mem))
    })
}

/// May `me` start now? A pure function of the machine reading and the queue,
/// so the status screen and the waiting job always agree.
pub fn decide(
    m: &MachineSample,
    lim: &Limits,
    running: &[Entry],
    waiting: &[Entry],
    me: &Entry,
    now: f64,
    unknown_starts: &[f64],
) -> Decision {
    let older: Vec<&Entry> = waiting.iter().filter(|w| w.ticket < me.ticket).collect();

    // The kernel's own alarm comes first and holds back every job, even when
    // taskguard runs nothing: then the pressure comes from other programs, and
    // one more job only makes it worse. This cannot deadlock the queue: jobs
    // start again as soon as the pressure eases, and --now still skips it.
    if let Some(p) = m.mem_pressure.filter(|p| *p >= lim.pressure_max) {
        return Decision::Wait { blockers: vec![Blocker::Pressure { level: p, limit: lim.pressure_max }] };
    }

    // Always make progress: with nothing running, the oldest job starts,
    // whatever the readings say. This is what keeps the queue from deadlocking
    // on a machine that is busy with work outside taskguard.
    if running.is_empty() {
        return match older.first() {
            None => Decision::Admit { reason: "nothing else is running".into() },
            Some(o) => Decision::Wait { blockers: vec![Blocker::Older { key: o.key.clone() }] },
        };
    }

    let room = Room::new(m, lim, running);

    let mut blockers = Vec::new();
    // An older job that newer ones have passed for `max_bypass` goes first.
    // Within `max_backfill` it holds its turn only while it could start
    // itself: a job that waits for memory cannot use the room it would hold,
    // so newer jobs that fit may start meanwhile. Past `max_backfill` it holds
    // its turn anyway, so a stream of small jobs cannot keep it out forever.
    if let Some(r) = older.iter().find(|w| {
        let waited = now - w.queued_at;
        w.bypassed_since.is_some()
            && waited > lim.max_bypass
            && (waited > lim.max_backfill || room.blockers(m, lim, running, w, now, unknown_starts).is_empty())
    }) {
        blockers.push(Blocker::Reserved { key: r.key.clone(), waited_s: now - r.queued_at });
    }
    blockers.extend(room.blockers(m, lim, running, me, now, unknown_starts));
    if blockers.is_empty() {
        let cpu = if short(lim, me) {
            format!("CPU not checked for a job that usually takes {:.1}s", me.est_dur_s.unwrap_or(0.0))
        } else {
            format!("CPU {:.1}+{:.1}+{:.1} of {:.1} cores", m.cpu_busy, room.res_cpu, me.need_cpu, room.cpu_limit)
        };
        let mem_would = room.mem_would(me);
        Decision::Admit { reason: format!("fits: {cpu}, memory {:.0}% of {:.0}%", pct(mem_would, m.mem_total_kb as f64), lim.mem_max_pct) }
    } else {
        Decision::Wait { blockers }
    }
}

/// The room the running jobs leave, read once per decision.
struct Room {
    res_cpu: f64,
    res_mem: u64,
    cpu_limit: f64,
    mem_limit: f64,
    mem_used: u64,
}

impl Room {
    fn new(m: &MachineSample, lim: &Limits, running: &[Entry]) -> Room {
        let (res_cpu, res_mem) = reserve(running);
        Room {
            res_cpu,
            res_mem,
            cpu_limit: m.ncpu as f64 * lim.cpu_max_pct / 100.0,
            mem_limit: m.mem_total_kb as f64 * lim.mem_max_pct / 100.0,
            mem_used: m.mem_for_admission(running.iter().map(|e| e.live_mem_kb).sum()),
        }
    }

    fn mem_would(&self, job: &Entry) -> f64 {
        (self.mem_used + self.res_mem + job.need_mem_kb) as f64
    }

    /// What keeps `job` itself from starting now, apart from the jobs ahead
    /// of it in line.
    fn blockers(&self, m: &MachineSample, lim: &Limits, running: &[Entry], job: &Entry, now: f64, unknown_starts: &[f64]) -> Vec<Blocker> {
        let mut blockers = Vec::new();
        if let (Some(pk), Some(max)) = (&job.pool_key, job.pool_slots) {
            let holders: Vec<String> = running.iter().filter(|e| e.pool_key.as_ref() == Some(pk)).map(|e| e.key.clone()).collect();
            if holders.len() as u32 >= max {
                blockers.push(Blocker::Slots { pool: job.pool.clone().unwrap_or_default(), busy: holders.len() as u32, max, holders });
            }
        }
        // Jobs with no history start in small batches, and a batch only starts
        // once the jobs of the one before have run for `learn_stagger` seconds.
        // Their needs are estimates until then, and the readings need time to show
        // what the new jobs really take. A batch is as large as the free room
        // allows: one job per free core, and as many as fit in the free memory at
        // this job's estimated need.
        if job.guessed()
            && let Some((r, wait_s)) = running.iter().filter_map(|r| r.settling_for(now).map(|w| (r, w))).min_by(|a, b| a.1.total_cmp(&b.1))
        {
            blockers.push(Blocker::Settling { key: r.key.clone(), wait_s });
        }
        if !job.known && !job.raised_by_min {
            let recent: Vec<f64> = unknown_starts.iter().copied().filter(|t| now - t < lim.learn_stagger).collect();
            let free_cpu = self.cpu_limit - m.cpu_busy - self.res_cpu;
            let free_mem_kb = self.mem_limit - (self.mem_used + self.res_mem) as f64;
            let per_job_kb = job.need_mem_kb.max(512 * 1024) as f64;
            let batch = free_cpu.min(free_mem_kb / per_job_kb).floor().max(1.0) as usize;
            if !recent.is_empty() && recent.len() >= batch {
                let newest = recent.iter().copied().fold(f64::MIN, f64::max);
                blockers.push(Blocker::LearnStagger { wait_s: (lim.learn_stagger - (now - newest)).max(0.0) });
            }
        }
        let mem_would = self.mem_would(job);
        if mem_would > self.mem_limit {
            blockers.push(Blocker::Memory {
                would_pct: pct(mem_would, m.mem_total_kb as f64),
                limit_pct: lim.mem_max_pct,
                short_kb: (mem_would - self.mem_limit) as u64,
                used_kb: self.mem_used,
                reserve_kb: self.res_mem,
                need_kb: job.need_mem_kb,
                total_kb: m.mem_total_kb,
            });
        }
        let cpu_would = m.cpu_busy + self.res_cpu + job.need_cpu;
        if !short(lim, job) && cpu_would > self.cpu_limit + 1e-9 {
            blockers.push(Blocker::Cpu {
                would: cpu_would,
                limit: self.cpu_limit,
                short: cpu_would - self.cpu_limit,
                busy: m.cpu_busy,
                reserve: self.res_cpu,
                need: job.need_cpu,
            });
        }
        blockers
    }
}

/// A job that usually ends within a few seconds is over before the CPU
/// reading could react to it, so holding it back only makes it late. Too
/// little CPU only slows a job down; memory stays a hard rule for all.
fn short(lim: &Limits, job: &Entry) -> bool {
    job.est_dur_s.is_some_and(|d| d < lim.cpu_min_duration)
}

fn pct(a: f64, b: f64) -> f64 {
    if b <= 0.0 { 0.0 } else { a * 100.0 / b }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024;
    const LIM: Limits = Limits {
        cpu_max_pct: 100.0,
        mem_max_pct: 85.0,
        learn_stagger: 2.0,
        max_bypass: 120.0,
        max_backfill: 0.0,
        cpu_min_duration: 5.0,
        pressure_max: 20.0,
    };

    /// A running entry exactly as v0.1.2 writes it.
    const ENTRY_V0_1_2: &str = r#"{"ticket":7,"pid":4242,"run_id":31,"key":"packages/api:tsc","ns":"shop","label":"typecheck","pool":null,"pool_key":null,"pool_slots":null,"checkout":"/w/shop","queued_at":1000.5,"started_at":1002.0,"need_cpu":3.0,"need_mem_kb":1468006,"known":true,"raised_by_min":false,"now":false,"live_cpu":1.2,"live_mem_kb":900000,"bypassed_since":null,"blocker":null,"blocker_since":null,"est_dur_s":20.0,"estimate_from":null}"#;

    #[test]
    fn entry_format_is_stable() {
        // This version reads what v0.1.2 wrote.
        let old: Entry = serde_json::from_str(ENTRY_V0_1_2).unwrap();
        assert_eq!((old.pid, old.need_mem_kb, old.started_at), (4242, 1468006, Some(1002.0)));

        // v0.1.2 reads what this version writes: every field it needs is still
        // there, with the same JSON type. Fields may only be added.
        let old: serde_json::Map<String, serde_json::Value> = serde_json::from_str(ENTRY_V0_1_2).unwrap();
        let new = serde_json::to_value(Entry { started_at: Some(1.0), ..Default::default() }).unwrap();
        let kind = |v: &serde_json::Value| match v {
            serde_json::Value::Number(n) if n.is_f64() => "float",
            serde_json::Value::Number(_) => "integer",
            serde_json::Value::Bool(_) => "bool",
            serde_json::Value::String(_) => "string",
            _ => "null or other",
        };
        for (field, value) in &old {
            let Some(now) = new.get(field) else { panic!("field {field} is gone; older versions need it") };
            if !value.is_null() && !now.is_null() {
                // Whole-number floats print as 1.0, so compare floats loosely.
                let (a, b) = (kind(value), kind(now));
                assert!(a == b || (a != "bool" && a != "string" && b != "bool" && b != "string"), "field {field} changed type: {a} -> {b}");
            }
        }
    }

    #[test]
    fn entries_from_other_versions_are_read() {
        // A newer version added a field; an older one left some out.
        let newer = ENTRY_V0_1_2.replace(r#""ticket":7"#, r#""ticket":7,"priority":3"#);
        assert_eq!(serde_json::from_str::<Entry>(&newer).unwrap().ticket, 7);
        let older: Entry = serde_json::from_str(r#"{"ticket":3,"pid":10,"run_id":1,"key":"k"}"#).unwrap();
        assert_eq!((older.ticket, older.need_cpu), (3, 0.0));
    }

    #[test]
    fn unreadable_entries_of_dead_owners_are_dropped() {
        let dir = std::env::temp_dir().join(format!("tg-reap-{}", std::process::id()));
        let q = Queue::open(&dir).unwrap();
        let dead = i32::MAX - 1;
        let live = std::process::id() as i32;
        fs::write(dir.join("wait").join(format!("9.{dead}")), "{not json").unwrap();
        fs::write(dir.join("run").join(dead.to_string()), r#"{"pid":"not a number"}"#).unwrap();
        fs::write(dir.join("run").join(live.to_string()), "{not json").unwrap();
        q.reap();
        assert!(!dir.join("wait").join(format!("9.{dead}")).exists());
        assert!(!dir.join("run").join(dead.to_string()).exists());
        assert!(dir.join("run").join(live.to_string()).exists(), "a live owner keeps its entry");
        fs::remove_dir_all(&dir).ok();
    }

    fn job(ticket: u64, key: &str, cpu: f64, mem_gb: u64) -> Entry {
        Entry { ticket, key: key.into(), need_cpu: cpu, need_mem_kb: mem_gb * GB, known: true, queued_at: 1000.0, ..Default::default() }
    }

    fn machine(busy: f64, used_gb: u64) -> MachineSample {
        MachineSample::fixed(busy, 12, used_gb * GB, 32 * GB)
    }

    fn blockers(d: Decision) -> Vec<&'static str> {
        match d {
            Decision::Admit { .. } => vec![],
            Decision::Wait { blockers } => blockers.iter().map(|b| b.name()).collect(),
        }
    }

    #[test]
    fn nothing_running_always_admits_the_oldest() {
        let big = job(1, "big", 64.0, 500);
        assert!(blockers(decide(&machine(12.0, 31), &LIM, &[], std::slice::from_ref(&big), &big, 1000.0, &[])).is_empty());
        let young = job(2, "young", 1.0, 1);
        assert_eq!(blockers(decide(&machine(0.0, 1), &LIM, &[], &[big.clone(), young.clone()], &young, 1000.0, &[])), vec!["order"]);
    }

    #[test]
    fn memory_and_cpu_include_the_reserve() {
        let mut running = job(1, "api", 4.0, 18);
        running.live_cpu = 1.0;
        running.live_mem_kb = 5 * GB;
        let me = job(2, "worker", 2.0, 4);
        // 10 GB used + 13 GB still to be taken by api + 4 GB = 27 GB = 84% -> fits.
        assert!(blockers(decide(&machine(2.0, 10), &LIM, &[running.clone()], std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty());
        // 12 GB used -> 29 GB = 90% -> memory blocks.
        assert_eq!(
            blockers(decide(&machine(2.0, 12), &LIM, &[running.clone()], std::slice::from_ref(&me), &me, 1000.0, &[])),
            vec!["memory"]
        );
        // 8 busy + 3 still to come + 2 = 13 > 12 cores -> CPU blocks too.
        assert_eq!(
            blockers(decide(&machine(8.0, 12), &LIM, &[running.clone()], std::slice::from_ref(&me), &me, 1000.0, &[])),
            vec!["memory", "cpu"]
        );
    }

    #[test]
    fn a_small_job_passes_a_big_one() {
        let running = job(1, "r", 1.0, 1);
        let big = job(2, "big", 2.0, 30);
        let small = job(3, "small", 1.0, 1);
        let waiting = [big.clone(), small.clone()];
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, std::slice::from_ref(&running), &waiting, &big, 1000.0, &[])), vec!["memory"]);
        assert!(blockers(decide(&machine(1.0, 8), &LIM, &[running], &waiting, &small, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_job_passed_for_too_long_is_reserved() {
        let running = job(1, "r", 1.0, 1);
        let mut big = job(2, "big", 2.0, 30);
        big.bypassed_since = Some(1010.0);
        let small = job(3, "small", 1.0, 1);
        let waiting = [big.clone(), small.clone()];
        // Not yet past max_bypass.
        assert!(blockers(decide(&machine(1.0, 8), &LIM, std::slice::from_ref(&running), &waiting, &small, 1100.0, &[])).is_empty());
        // Past it: nobody newer may start.
        assert_eq!(blockers(decide(&machine(1.0, 8), &LIM, &[running], &waiting, &small, 1200.0, &[])), vec!["reserved"]);
    }

    /// A 20 GB compile has run since 1000 and holds its memory. At 1000 a
    /// 12 GB job arrived that does not fit next to it, and newer jobs have
    /// passed it since 1010.
    fn blocked_head() -> (Entry, Entry) {
        let mut running = job(1, "compile", 1.0, 20);
        running.started_at = Some(1000.0);
        running.live_mem_kb = 20 * GB;
        let mut head = job(2, "typecheck", 2.0, 12);
        head.bypassed_since = Some(1010.0);
        (running, head)
    }

    #[test]
    fn a_reserved_job_that_does_not_fit_lets_newer_jobs_that_fit_start() {
        let (running, head) = blocked_head();
        let small = job(3, "install", 0.2, 1);
        let waiting = [head.clone(), small.clone()];
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        // 20 GB held + 12 GB needed is more than 27.2 GB (85% of 32).
        let m = machine(2.0, 21);
        let r = std::slice::from_ref(&running);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &head, 1300.0, &[])), vec!["memory"]);
        // The small job fits and starts: the head could not use the room.
        assert!(blockers(decide(&m, &backfill, r, &waiting, &small, 1300.0, &[])).is_empty());
        // Without backfill, the head keeps its reservation, as before.
        assert_eq!(blockers(decide(&m, &LIM, r, &waiting, &small, 1300.0, &[])), vec!["reserved"]);

        // The compile shrinks to 5 GB: now the head fits, so it goes first.
        let mut shrunk = running.clone();
        shrunk.need_mem_kb = 5 * GB;
        shrunk.live_mem_kb = 5 * GB;
        let m = machine(2.0, 6);
        let r = std::slice::from_ref(&shrunk);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &small, 1300.0, &[])), vec!["reserved"]);
        assert!(blockers(decide(&m, &backfill, r, &waiting, &head, 1300.0, &[])).is_empty());
    }

    #[test]
    fn past_max_backfill_the_reservation_holds_even_when_the_job_does_not_fit() {
        let (running, head) = blocked_head();
        let small = job(3, "install", 0.2, 1);
        let waiting = [head.clone(), small.clone()];
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        let m = machine(2.0, 21);
        let r = std::slice::from_ref(&running);
        assert!(blockers(decide(&m, &backfill, r, &waiting, &small, 2799.0, &[])).is_empty());
        // Waiting longer than 1800 s: newer jobs stop, so memory drains until
        // the head fits.
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &small, 2801.0, &[])), vec!["reserved"]);
        // A window no longer than max_bypass is no window: the old rule.
        let none = Limits { max_backfill: 120.0, ..LIM };
        assert_eq!(blockers(decide(&m, &none, r, &waiting, &small, 1300.0, &[])), vec!["reserved"]);
    }

    #[test]
    fn backfill_keeps_pools_and_pressure() {
        let (mut running, mut head) = blocked_head();
        let backfill = Limits { max_backfill: 1800.0, ..LIM };
        // The head is held only by its pool slot: newer jobs outside the pool
        // start, newer jobs in the same pool still wait for the slot.
        running.need_mem_kb = GB;
        running.live_mem_kb = GB;
        running.pool_key = Some("db@/repo".into());
        head.pool = Some("db".into());
        head.pool_key = Some("db@/repo".into());
        head.pool_slots = Some(1);
        let mut same_pool = job(3, "migrate", 0.2, 1);
        same_pool.pool = head.pool.clone();
        same_pool.pool_key = head.pool_key.clone();
        same_pool.pool_slots = Some(1);
        let other = job(4, "lint", 0.2, 1);
        let waiting = [head.clone(), same_pool.clone(), other.clone()];
        let m = machine(2.0, 2);
        let r = std::slice::from_ref(&running);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &head, 1300.0, &[])), vec!["slots"]);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &same_pool, 1300.0, &[])), vec!["slots"]);
        assert!(blockers(decide(&m, &backfill, r, &waiting, &other, 1300.0, &[])).is_empty());
        // Memory pressure still stops every new job.
        let mut m = m;
        m.mem_pressure = Some(50.0);
        assert_eq!(blockers(decide(&m, &backfill, r, &waiting, &other, 1300.0, &[])), vec!["pressure"]);
    }

    #[test]
    fn pools_count_slots_per_pool_key() {
        let mut running = job(1, "e2e-a", 0.1, 0);
        running.pool_key = Some("e2e@/repo".into());
        let mut me = job(2, "e2e-b", 0.1, 0);
        me.pool = Some("e2e".into());
        me.pool_key = Some("e2e@/repo".into());
        me.pool_slots = Some(1);
        assert_eq!(blockers(decide(&machine(0.0, 1), &LIM, &[running.clone()], &[me.clone()], &me, 1000.0, &[])), vec!["slots"]);
        me.pool_key = Some("e2e@/other-worktree".into());
        assert!(blockers(decide(&machine(0.0, 1), &LIM, &[running], &[me.clone()], &me, 1000.0, &[])).is_empty());
    }

    #[test]
    fn a_first_run_waits_until_running_first_runs_settle() {
        // A first run started at 1000 and still grows; its memory last rose at 1030.
        let mut growing = job(1, "packages/ci:run", 1.0, 2);
        growing.known = false;
        growing.started_at = Some(1000.0);
        growing.mem_grew_at = Some(1030.0);
        let mut me = job(2, "new", 1.0, 1);
        me.known = false;
        let r = std::slice::from_ref(&growing);
        let w = std::slice::from_ref(&me);
        let d = decide(&machine(1.0, 4), &LIM, r, w, &me, 1040.0, &[]);
        assert_eq!(blockers(d.clone()), vec!["learning"]);
        assert!(
            matches!(&d, Decision::Wait { blockers } if matches!(&blockers[0], Blocker::Settling { wait_s, .. } if (*wait_s - 10.0).abs() < 1e-9))
        );
        // Steady for 20 s: it has shown what it takes.
        assert!(blockers(decide(&machine(1.0, 4), &LIM, r, w, &me, 1051.0, &[])).is_empty());
        // Still growing, but past two minutes: it no longer holds new jobs back.
        let mut late = growing.clone();
        late.mem_grew_at = Some(1119.0);
        assert!(blockers(decide(&machine(1.0, 4), &LIM, std::slice::from_ref(&late), w, &me, 1121.0, &[])).is_empty());
        // A job with history is never held back by the rule.
        let known = job(3, "old", 1.0, 1);
        assert!(blockers(decide(&machine(1.0, 4), &LIM, r, std::slice::from_ref(&known), &known, 1040.0, &[])).is_empty());
    }

    #[test]
    fn overage_is_measured_against_the_needs_a_job_started_with() {
        let mut e = job(1, "k", 2.0, 4);
        e.live_mem_kb = 6 * GB;
        e.need_mem_kb = 7 * GB + GB / 2; // raised to follow its peak
        e.start_need_mem_kb = Some(4 * GB);
        assert_eq!(over(&[e]).1, 2 * GB);
    }

    #[test]
    fn unknown_jobs_start_in_batches_sized_by_free_room() {
        let running = job(1, "r", 1.0, 1);
        let mut me = job(2, "new", 0.0, 0);
        me.known = false;
        let r = std::slice::from_ref(&running);
        let w = std::slice::from_ref(&me);
        // A nearly full machine: batches of one, two seconds apart.
        assert_eq!(blockers(decide(&machine(10.5, 1), &LIM, r, w, &me, 1001.0, &[1000.0])), vec!["learning"]);
        assert!(blockers(decide(&machine(10.5, 1), &LIM, r, w, &me, 1003.0, &[1000.0])).is_empty());
        // An idle machine: many new jobs may start in the same window.
        let three = [1000.0, 1000.5, 1001.0];
        assert!(blockers(decide(&machine(1.0, 1), &LIM, r, w, &me, 1001.2, &three)).is_empty());
        // Memory limits the batch too: 26 of 27.2 usable GB held leaves room for one.
        assert_eq!(blockers(decide(&machine(1.0, 26), &LIM, r, w, &me, 1001.2, &three)), vec!["learning"]);
    }

    /// Replays the freeze of 2026-09-25: a first typecheck of a whole monorepo.
    /// 60 compiles with no history arrive at once on a 32 GB machine that
    /// already holds 16 GB. Each really grows to between 0.3 and 4.2 GB over
    /// 8 seconds and runs for 30. The kernel raises its pressure level as
    /// memory fills. Returns the highest share of memory in use.
    fn replay_burst(lim: &Limits, estimate_kb: u64) -> f64 {
        const TOTAL: u64 = 32 * GB;
        const BASE: u64 = 16 * GB;
        let peaks: Vec<u64> = (0..60).map(|i| [300, 700, 4200, 500, 1200, 900][i % 6] * 1024).collect();
        let mut waiting: Vec<Entry> = (0..60)
            .map(|i| Entry {
                ticket: i + 1,
                key: format!("pkg{i}:tsc"),
                need_mem_kb: estimate_kb,
                known: false,
                queued_at: 0.0,
                ..Default::default()
            })
            .collect();
        let mut running: Vec<(Entry, f64, u64)> = Vec::new(); // entry, start, real peak
        let mut starts: Vec<f64> = Vec::new();
        let mut recent_mem: Vec<(f64, u64, u64)> = Vec::new();
        let mut worst: f64 = 0.0;
        let mut t = 0.0;
        while t < 400.0 && (!waiting.is_empty() || !running.is_empty()) {
            running.retain(|(_, s, _)| t - s < 30.0);
            for (e, s, peak) in running.iter_mut() {
                e.live_mem_kb = (*peak as f64 * ((t - *s) / 8.0).min(1.0)) as u64;
            }
            let used = BASE + running.iter().map(|(e, _, _)| e.live_mem_kb).sum::<u64>();
            worst = worst.max(used as f64 * 100.0 / TOTAL as f64);
            recent_mem.retain(|(ts, _, _)| t - ts < crate::machine::PEAK_WINDOW);
            recent_mem.push((t, used, used - BASE));
            let pct = used as f64 * 100.0 / TOTAL as f64;
            let pressure = if pct > 90.0 {
                100.0
            } else if pct > 80.0 {
                50.0
            } else {
                0.0
            };
            let m = MachineSample {
                ts: t,
                cpu_busy: 2.0,
                ncpu: 10,
                mem_used_kb: used,
                mem_total_kb: TOTAL,
                mem_pressure: Some(pressure),
                recent_mem: recent_mem.clone(),
                ..Default::default()
            };
            // Every waiting job checks once per tick, oldest first, as the runners do.
            let mut i = 0;
            while i < waiting.len() {
                let run: Vec<Entry> = running.iter().map(|(e, _, _)| e.clone()).collect();
                let me = waiting[i].clone();
                if matches!(decide(&m, lim, &run, &waiting, &me, t, &starts), Decision::Admit { .. }) {
                    let peak = peaks[me.ticket as usize - 1];
                    starts.push(t);
                    running.push((me, t, peak));
                    waiting.remove(i);
                } else {
                    i += 1;
                }
            }
            t += 0.5;
        }
        assert!(waiting.is_empty(), "every job still ran");
        worst
    }

    #[test]
    fn a_burst_of_first_runs_no_longer_fills_the_machine() {
        // The rules at the time of the freeze: a first run needed nothing,
        // batches every 2 s, and the kernel's pressure alarm was ignored.
        let old = Limits { learn_stagger: 2.0, pressure_max: f64::MAX, ..LIM };
        let before = replay_burst(&old, 0);
        assert!(before > 97.0, "the old rules fill the machine: {before:.0}%");
        // Now: an estimated need, settled batches, and the pressure stop.
        let new = Limits { learn_stagger: 5.0, pressure_max: 20.0, ..LIM };
        let after = replay_burst(&new, 1536 * 1024);
        eprintln!("burst replay: old rules peak at {before:.0}% memory, new rules at {after:.0}%");
        assert!(after < 90.0, "the new rules stay clear of the edge: {after:.0}%");
    }

    #[test]
    fn pressure_stops_every_new_job() {
        let running = job(1, "r", 1.0, 1);
        let me = job(2, "tsc", 1.0, 1);
        let mut m = machine(1.0, 8);
        m.mem_pressure = Some(50.0);
        assert_eq!(
            blockers(decide(&m, &LIM, std::slice::from_ref(&running), std::slice::from_ref(&me), &me, 1000.0, &[])),
            vec!["pressure"]
        );
        // Even with nothing of ours running: the pressure comes from others.
        assert_eq!(blockers(decide(&m, &LIM, &[], std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["pressure"]);
        m.mem_pressure = Some(0.0);
        assert!(blockers(decide(&m, &LIM, &[], std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty(), "it eases: the job starts");
    }

    #[test]
    fn admission_uses_the_peak_of_the_last_seconds() {
        let running = job(1, "r", 1.0, 1);
        let me = job(2, "tsc", 1.0, 2);
        let mut m = machine(1.0, 20);
        // It read 26 GB a few seconds ago; the dip to 20 GB now does not count.
        m.recent_mem = vec![(995.0, 26 * GB, 0), (999.0, 20 * GB, 0)];
        m.ts = 1000.0;
        assert_eq!(blockers(decide(&m, &LIM, std::slice::from_ref(&running), std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["memory"]);
    }

    #[test]
    fn short_jobs_skip_the_cpu_check_but_not_memory() {
        let running = job(1, "r", 1.0, 1);
        let r = std::slice::from_ref(&running);
        let mut me = job(2, "lint", 2.0, 1);
        me.est_dur_s = Some(0.4);
        assert!(blockers(decide(&machine(12.0, 8), &LIM, r, std::slice::from_ref(&me), &me, 1000.0, &[])).is_empty());
        assert_eq!(blockers(decide(&machine(12.0, 27), &LIM, r, std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["memory"]);
        me.est_dur_s = Some(30.0);
        assert_eq!(blockers(decide(&machine(12.0, 8), &LIM, r, std::slice::from_ref(&me), &me, 1000.0, &[])), vec!["cpu"]);
    }
}
