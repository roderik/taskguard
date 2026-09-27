//! Running one job: take a ticket, wait for room, run the command, measure it,
//! learn from it, and say what happened.

use crate::config::{self, Config};
use crate::db::{self, Db, NewRun, RunResult};
use crate::insight::{self, AdviceInput, RunFacts, Tracker};
use crate::key;
use crate::machine;
use crate::matcher;
use crate::queue::{self, Decision, Entry, Limits, Queue};
use crate::report::{self, Lines, say};
use crate::sys;
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Options from the sem-style command line.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Opts {
    pub jobs: Option<String>,
    pub id: Option<String>,
    pub ns: Option<String>,
    pub timeout: Option<f64>,
    /// Exit code when a negative --st gives up; 124 (as `timeout`) by default.
    pub timeout_exit: Option<i32>,
    pub key: Option<String>,
    pub min_cpu: Option<f64>,
    pub min_mem_kb: Option<u64>,
    pub now: bool,
    pub bg: bool,
    pub pipe: bool,
    pub quiet: bool,
    pub hints: Option<bool>,
    pub wait: bool,
    pub cmd: Vec<String>,
}

/// sem's `-j`: N, +N (cores + N), -N (cores - N) or N% of cores, at least 1.
pub fn parse_jobs(s: &str, ncpu: usize) -> Result<u32> {
    let n = ncpu as f64;
    let v = if let Some(p) = s.strip_suffix('%') {
        n * p.parse::<f64>().with_context(|| format!("bad -j {s:?}"))? / 100.0
    } else if let Some(p) = s.strip_prefix('+') {
        n + p.parse::<f64>().with_context(|| format!("bad -j {s:?}"))?
    } else if s.starts_with('-') {
        n + s.parse::<f64>().with_context(|| format!("bad -j {s:?}"))?
    } else {
        s.parse::<f64>().with_context(|| format!("bad -j {s:?}"))?
    };
    Ok(v.floor().max(1.0) as u32)
}

const POLL: Duration = Duration::from_millis(100);

/// Needs set by hand hold for this run only. The run is still measured, so
/// the next run learns from what it really used.
fn apply_nudge(me: &mut Entry, n: &queue::Nudge, database: Option<&Db>) {
    if n.need_cpu.is_none() && n.need_mem_kb.is_none() {
        return;
    }
    let (cpu, mem) = (n.need_cpu.unwrap_or(me.need_cpu), n.need_mem_kb.unwrap_or(me.need_mem_kb));
    me.by_hand =
        Some(format!("set by hand to {cpu:.1} cores, {} (was {:.1} cores, {})", report::gb(mem), me.need_cpu, report::gb(me.need_mem_kb)));
    me.need_cpu = cpu;
    me.need_mem_kb = mem;
    // The job's needs are now known: no estimate, no learn stagger.
    me.known = true;
    if let Some(d) = database {
        let _ = d.set_need(me.run_id, cpu, mem);
    }
}

fn cleanup_files(q: &Queue, e: &Entry) {
    let _ = std::fs::remove_file(q.wait_path(e));
    let _ = std::fs::remove_file(q.run_path(e.pid));
}

/// Is this call nested inside a job that already holds a slot? The variable
/// covers the usual case. The ancestor check covers a task runner in between
/// that drops unknown variables (turbo's strict env mode): without it, an inner
/// job would wait for room that is reserved for its own parent, forever.
fn held_by_ancestor(q: &Queue) -> bool {
    if std::env::var("TASKGUARD_HELD").is_ok_and(|v| v == "1") {
        return true;
    }
    let procs = sys::list_procs();
    let me = std::process::id() as i32;
    sys::ancestors(me, &procs).iter().skip(1).any(|p| q.run_path(*p).exists())
}

/// Start the recorder unless one runs already. The recorder holds an flock on
/// recorder.lock for its whole life, so a free lock means none is running.
pub fn ensure_recorder(dir: &Path) {
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("recorder.lock")) else {
        return;
    };
    if f.try_lock().is_err() {
        return;
    }
    drop(f);
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = Command::new(exe);
    cmd.arg("__recorder").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

struct Signals {
    int: Arc<AtomicBool>,
    term: Arc<AtomicBool>,
    hup: Arc<AtomicBool>,
}

impl Signals {
    fn register() -> Signals {
        let s = Signals { int: Arc::default(), term: Arc::default(), hup: Arc::default() };
        let _ = signal_hook::flag::register(libc::SIGINT, s.int.clone());
        let _ = signal_hook::flag::register(libc::SIGTERM, s.term.clone());
        let _ = signal_hook::flag::register(libc::SIGHUP, s.hup.clone());
        s
    }

    fn any(&self) -> Option<i32> {
        if self.term.load(Ordering::Relaxed) {
            Some(libc::SIGTERM)
        } else if self.hup.load(Ordering::Relaxed) {
            Some(libc::SIGHUP)
        } else if self.int.load(Ordering::Relaxed) {
            Some(libc::SIGINT)
        } else {
            None
        }
    }
}

fn exec(cmd: &[String]) -> ! {
    let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
    eprintln!("taskguard: cannot run {}: {err}", cmd[0]);
    std::process::exit(127);
}

pub fn run(mut o: Opts) -> Result<i32> {
    if o.cmd.is_empty() {
        bail!("no command given");
    }
    let dir = config::state_dir();
    let q = Queue::open(&dir)?;
    if std::env::var("TASKGUARD_DISABLE").is_ok_and(|v| v == "1") || held_by_ancestor(&q) {
        exec(&o.cmd);
    }

    let cwd = std::env::current_dir().context("reading the current directory")?;
    let checkout = key::checkout_root(&cwd);
    let cfg = Config::load(&cwd, &checkout)?;
    let jkey = o.key.clone().unwrap_or_else(|| key::run_key(&cwd, &o.cmd));
    let cls = cfg.classify(&o.cmd, &jkey)?;
    if cls.passthrough {
        exec(&o.cmd);
    }
    if o.bg {
        return run_bg(&o);
    }

    let hints = o.hints.unwrap_or(cfg.hints);
    let lines = Lines { hints, quiet: o.quiet };
    o.now |= cls.now;
    let min_cpu = o.min_cpu.or(cls.min_cpu);
    let min_mem = o.min_mem_kb.or(cls.min_mem_kb);
    let ns = o.ns.clone().unwrap_or_else(|| key::namespace(&cwd));

    // The pool: an explicit --id / -j wins over a matched built-in pool.
    let ncpu = sys::ncpu();
    let (pool, pool_slots, per_checkout) = match (&o.id, &o.jobs) {
        (Some(id), j) => {
            let from_cfg = cfg.pools.iter().find(|p| &p.name == id);
            let slots = match j {
                Some(j) => Some(parse_jobs(j, ncpu)?),
                None => from_cfg.and_then(|p| p.max_slots),
            };
            (Some(id.clone()), slots, from_cfg.map(|p| p.per_checkout).unwrap_or(false))
        }
        (None, Some(j)) => (Some("default".to_string()), Some(parse_jobs(j, ncpu)?), false),
        (None, None) => match &cls.pool {
            Some((n, s, pc)) => (Some(n.clone()), *s, *pc),
            None => (None, None, false),
        },
    };
    let pool_key = pool.as_ref().map(|p| if per_checkout { format!("{p}@{}", checkout.display()) } else { p.clone() });

    let database = Db::open_dir(&dir).ok();
    let learned = database.as_ref().and_then(|d| d.learned(&jkey, cfg.hist_keep, cfg.boost_runs).ok()).unwrap_or_default();
    // A compile with no history of its own falls back on what tsc-queue learned.
    let tool = matcher::effective(&o.cmd).first().map(|c| c.rsplit('/').next().unwrap_or(c).to_string()).unwrap_or_default();
    let learned = match (&database, learned.runs, tool.as_str()) {
        (Some(d), 0, "tsc" | "tsgo") => {
            d.learned(&format!("tsc-queue:{}", crate::commands::legacy_key(&cwd)), cfg.hist_keep, cfg.boost_runs).unwrap_or(learned)
        }
        _ => learned,
    };
    let learned_mem = learned.mem_kb.map(|m| if learned.mem_boosted { (m as f64 * 1.25) as u64 } else { m });
    // A job that once wanted more cores than the machine has is capped at the
    // limit; otherwise it could only ever start on an empty machine.
    let cpu_cap = sys::ncpu() as f64 * cfg.cpu_max / 100.0;
    // A job with no history still reserves room: what similar jobs needed, or
    // new_job_mem without any. Counting a first run as zero let a burst of new
    // compiles start into a machine that could not hold them, and froze it:
    // each grew to several GB within seconds, faster than the readings showed.
    let (est_mem, est_cpu, estimate_from) = if learned.runs == 0 {
        let est = database.as_ref().and_then(|d| d.estimate(cls.label.as_deref(), &tool, pool.as_deref()).ok().flatten());
        let from = match &est {
            Some(e) if e.mem_kb.is_some() => e.from.clone(),
            _ => "the new_job_mem default".to_string(),
        };
        let (m, c) = est.map(|e| (e.mem_kb, e.cpu)).unwrap_or((None, None));
        (Some(m.unwrap_or(cfg.new_job_mem_kb)), Some(c.unwrap_or(1.0)), Some(from))
    } else {
        (None, None, None)
    };
    let need_cpu = learned.cpu.or(est_cpu).unwrap_or(0.0).min(cpu_cap).max(min_cpu.unwrap_or(0.0));
    let need_mem = learned_mem.or(est_mem).unwrap_or(0).max(min_mem.unwrap_or(0));
    let raised = min_cpu.is_some_and(|m| m > learned.cpu.unwrap_or(0.0)) || min_mem.is_some_and(|m| m > learned_mem.unwrap_or(0));

    // `bunx` and `npx` set the variable to their own name; that is no script.
    let script = std::env::var("npm_lifecycle_event").ok().filter(|s| !matches!(s.as_str(), "bunx" | "npx" | "pnpx" | "dlx" | "exec"));
    // Both sides are resolved first: on macOS the cwd comes back as
    // /private/var/... while the variable may say /var/....
    let real_checkout = std::fs::canonicalize(&checkout).unwrap_or(checkout.clone());
    let package_json = std::env::var("npm_package_json").ok().map(|p| {
        let pp = Path::new(&p);
        // The file itself may be gone; its directory is enough.
        let real = std::fs::canonicalize(pp).unwrap_or_else(|_| {
            pp.parent()
                .and_then(|d| std::fs::canonicalize(d).ok())
                .map(|d| d.join(pp.file_name().unwrap_or_default()))
                .unwrap_or_else(|| pp.to_path_buf())
        });
        real.strip_prefix(&real_checkout).map(|r| r.display().to_string()).unwrap_or(p)
    });
    let me_pid = std::process::id() as i32;
    let cmdline = o.cmd.join(" ");
    let run_id = database
        .as_ref()
        .and_then(|d| {
            d.insert_run(&NewRun {
                ns: &ns,
                pool: pool.as_deref(),
                key: &jkey,
                label: cls.label.as_deref(),
                cwd: &cwd.to_string_lossy(),
                cmd: &cmdline,
                pid: me_pid,
                script: script.as_deref(),
                package_json: package_json.as_deref(),
                now: o.now,
                min_cpu,
                min_mem_kb: min_mem,
                need_cpu,
                need_mem_kb: need_mem,
            })
            .ok()
        })
        .unwrap_or(0);

    let mut me = Entry {
        ticket: 0,
        pid: me_pid,
        run_id,
        key: jkey.clone(),
        ns: ns.clone(),
        label: cls.label.clone(),
        pool: pool.clone(),
        pool_key,
        pool_slots,
        checkout: checkout.display().to_string(),
        queued_at: db::now(),
        need_cpu,
        need_mem_kb: need_mem,
        known: learned.runs > 0,
        estimate_from,
        raised_by_min: raised,
        now: o.now,
        est_dur_s: learned.dur_s,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        ..Default::default()
    };

    ensure_recorder(&dir);
    let sig = Signals::register();
    let limits_of = |cfg: &Config| Limits {
        cpu_max_pct: cfg.cpu_max,
        mem_max_pct: cfg.mem_max,
        learn_stagger: cfg.learn_stagger,
        max_bypass: cfg.max_bypass as f64,
        max_backfill: cfg.max_backfill as f64,
        cpu_min_duration: cfg.cpu_min_duration,
        pressure_max: cfg.pressure_max,
    };
    let mut limits = limits_of(&cfg);
    // A limit changed in the dashboard's Config view reaches jobs that
    // already wait: they read the user config again when it changes.
    let conf_path = config::user_config_path();
    let conf_mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut seen_mtime = conf_mtime(&conf_path);

    // ---- wait for room
    let t0 = db::now();
    let mut main_blocker: Option<String> = None;
    let admit_reason: String;
    {
        let _g = q.lock()?;
        me.ticket = q.next_ticket()?;
        if o.now {
            me.started_at = Some(t0);
            q.write(&q.run_path(me_pid), &me)?;
        } else {
            q.write(&q.wait_path(&me), &me)?;
        }
        q.touch_activity();
    }
    if o.now {
        lines.now(&me);
        admit_reason = "queue skipped on request".into();
    } else {
        let mut announced = false;
        let mut last_line = t0;
        let mut span: Option<(String, String, f64)> = None;
        let mut started_by_hand = false;
        loop {
            if let Some(s) = sig.any() {
                cleanup_files(&q, &me);
                if let Some(d) = &database {
                    let _ = d.abandon_run(run_id);
                }
                return Ok(128 + s);
            }
            let now = db::now();
            let mtime = conf_mtime(&conf_path);
            if mtime != seen_mtime {
                seen_mtime = mtime;
                if let Ok(c) = Config::load(&cwd, &checkout) {
                    limits = limits_of(&c);
                }
            }
            let decision;
            let running;
            {
                let _g = q.lock()?;
                for gone in q.reap() {
                    if let Some(d) = &database {
                        let _ = d.abandon_run(gone);
                    }
                }
                running = q.running();
                let m = machine::current(&dir, 1.0, running.iter().map(|e| e.live_mem_kb).sum());
                let waiting = q.waiting();
                // Keep fields another process may have set (bypassed_since).
                if let Some(cur) = waiting.iter().find(|w| w.pid == me_pid) {
                    me.bypassed_since = cur.bypassed_since;
                }
                if let Some(n) = q.take_nudge(me_pid) {
                    apply_nudge(&mut me, &n, database.as_ref());
                    started_by_hand |= n.start;
                }
                let mut waiting = waiting;
                if let Some(w) = waiting.iter_mut().find(|w| w.pid == me_pid) {
                    w.need_cpu = me.need_cpu;
                    w.need_mem_kb = me.need_mem_kb;
                }
                decision = queue::decide(&m, &limits, &running, &waiting, &me, now, &q.unknown_starts());
                let forced = started_by_hand || matches!(o.timeout, Some(t) if t > 0.0 && now - t0 >= t);
                if matches!(decision, Decision::Admit { .. }) || forced {
                    // Every older job that is still waiting has now been passed.
                    for w in waiting.iter().filter(|w| w.ticket < me.ticket && w.bypassed_since.is_none()) {
                        let mut w = w.clone();
                        w.bypassed_since = Some(now);
                        let _ = q.write(&q.wait_path(&w), &w);
                    }
                    if !me.known {
                        q.add_unknown_start(now, cfg.learn_stagger);
                    }
                    me.started_at = Some(now);
                    me.start_need_cpu = Some(me.need_cpu);
                    me.start_need_mem_kb = Some(me.need_mem_kb);
                    me.blocker = None;
                    q.write(&q.run_path(me_pid), &me)?;
                    let _ = std::fs::remove_file(q.wait_path(&me));
                    q.touch_activity();
                } else {
                    let text = report::main_blocker(&decision).map(report::blocker_text);
                    let name = report::main_blocker(&decision).map(|b| b.name());
                    if name != span.as_ref().map(|(n, _, _)| n.as_str()) || me.blocker_since.is_none() {
                        me.blocker_since = Some(now);
                    }
                    me.blocker = text;
                    q.write(&q.wait_path(&me), &me)?;
                }
            }
            let forced = matches!(o.timeout, Some(t) if t > 0.0 && now - t0 >= t);
            // Record each change of the main blocker as a span, for the dashboard.
            let cur = report::main_blocker(&decision).map(|b| (b.name().to_string(), report::blocker_text(b)));
            let changed = match (&span, &cur) {
                (Some((n, _, _)), Some((c, _))) => n != c,
                (None, None) => false,
                _ => true,
            };
            if changed {
                if let (Some((n, d, from)), Some(dbh)) = (&span, &database) {
                    let _ = dbh.wait_span(run_id, *from, now, n, d);
                }
                span = cur.map(|(n, d)| (n, d, now));
            }
            if let Some((n, _, _)) = &span {
                main_blocker = Some(n.clone());
            }
            match &decision {
                Decision::Admit { reason } => {
                    admit_reason = reason.clone();
                    break;
                }
                _ if started_by_hand => {
                    admit_reason = "started by hand from the dashboard".into();
                    break;
                }
                _ if forced => {
                    admit_reason = "the --st timeout ran out, so it runs anyway".into();
                    break;
                }
                _ => {}
            }
            if let Some(t) = o.timeout
                && t < 0.0
                && now - t0 >= -t
            {
                cleanup_files(&q, &me);
                if let Some(d) = &database {
                    let _ = d.abandon_run(run_id);
                }
                let code = o.timeout_exit.unwrap_or(124);
                say(&format!("timeout {} - gave up after {} in the queue, exit {code}", me.key, report::dur(now - t0)));
                return Ok(code);
            }
            // The queued line is only worth printing when the job really waits.
            if !announced {
                announced = true;
                lines.queued(&me, learned.runs, o.timeout);
            }
            if now - last_line >= cfg.status_every as f64 {
                last_line = now;
                let left = o.timeout.map(|t| if t > 0.0 { t - (now - t0) } else { t + (now - t0) });
                lines.waiting(&me, now - t0, &decision, &running, now, left);
            }
            std::thread::sleep(POLL);
        }
        if let (Some((n, d, from)), Some(dbh)) = (&span, &database) {
            let _ = dbh.wait_span(run_id, *from, db::now(), n, d);
        }
    }
    let started = me.started_at.unwrap_or_else(db::now);
    let waited = started - t0;
    if let Some(d) = &database {
        let _ = d.mark_started(run_id, started, waited, main_blocker.as_deref());
    }
    if !o.now {
        lines.start(&me, waited, &admit_reason);
    }
    // Tell a --bg parent that the job has started.
    if let Ok(fd) = std::env::var("TASKGUARD_BG_FD")
        && let Ok(fd) = fd.parse::<i32>()
    {
        unsafe {
            libc::write(fd, b"1".as_ptr() as *const libc::c_void, 1);
            libc::close(fd);
        }
    }

    // ---- run and measure
    let mut child = match Command::new(&o.cmd[0]).args(&o.cmd[1..]).env("TASKGUARD_HELD", "1").env_remove("TASKGUARD_BG_FD").spawn() {
        Ok(c) => c,
        Err(e) => {
            cleanup_files(&q, &me);
            if let Some(d) = &database {
                let _ = d.abandon_run(run_id);
            }
            eprintln!("taskguard: cannot run {}: {e}", o.cmd[0]);
            return Ok(127);
        }
    };
    let child_pid = child.id() as i32;
    // No reading at the very start: a job holds almost nothing in its first
    // instant, and a job shorter than one interval is covered by the final
    // reading below instead.
    let mut tracker = Tracker::starting_at(started);
    let mut last_sample = started;
    let mut last_mem_kb = 0u64;
    let mut seen_peak_kb = 0u64;
    let mut int_seen: Option<f64> = None;
    let mut forwarded = [false; 3];
    // wait4 rather than Child::try_wait: it also returns the kernel's own
    // totals for the child, which measure a job too short to be sampled.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let status = loop {
        let mut raw = 0;
        let r = unsafe { libc::wait4(child_pid, &mut raw, libc::WNOHANG, &mut ru) };
        if r == child_pid {
            break std::process::ExitStatus::from_raw(raw);
        }
        if r < 0 {
            break child.wait()?;
        }
        let now = db::now();
        // TERM and HUP are passed on at once. INT usually reaches the child from
        // the terminal already, since both share the foreground process group;
        // it is only passed on if the child is still alive a moment later, which
        // covers a parent that signals this process alone.
        if sig.term.load(Ordering::Relaxed) && !forwarded[0] {
            forwarded[0] = true;
            unsafe { libc::kill(child_pid, libc::SIGTERM) };
        }
        if sig.hup.load(Ordering::Relaxed) && !forwarded[1] {
            forwarded[1] = true;
            unsafe { libc::kill(child_pid, libc::SIGHUP) };
        }
        if sig.int.load(Ordering::Relaxed) && !forwarded[2] {
            let seen = *int_seen.get_or_insert(now);
            if now - seen > 0.5 {
                forwarded[2] = true;
                unsafe { libc::kill(child_pid, libc::SIGINT) };
            }
        }
        if now - last_sample >= cfg.sample_every {
            let span = now - last_sample;
            last_sample = now;
            if let Some(n) = q.take_nudge(me_pid) {
                apply_nudge(&mut me, &n, database.as_ref());
            }
            if let Some(r) = tracker.sample(child_pid, now) {
                me.live_cpu = r.used;
                me.live_mem_kb = r.mem_kb;
                if r.mem_kb > seen_peak_kb + seen_peak_kb / 10 {
                    me.mem_grew_at = Some(now);
                }
                seen_peak_kb = seen_peak_kb.max(r.mem_kb);
                // Past its needs, a job's needs follow its peak, with room to
                // grow: what it promises to take must keep up with what it takes.
                if r.mem_kb > me.need_mem_kb {
                    me.need_mem_kb = r.mem_kb + r.mem_kb / 4;
                }
                if r.wanted > me.need_cpu {
                    me.need_cpu = r.wanted.min(sys::ncpu() as f64);
                }
                last_mem_kb = r.mem_kb;
                let _ = q.write(&q.run_path(me_pid), &me);
                if let Some(d) = &database {
                    let _ = d.job_sample(run_id, now, span, r.used, r.wanted, r.mem_kb, r.pageins_per_s);
                }
                if let Some(m) = machine::read_cache(&dir) {
                    let cpu_full = m.cpu_busy >= 0.95 * m.ncpu as f64 * cfg.cpu_max / 100.0;
                    let mem_full = m.mem_pct() >= cfg.mem_max;
                    if cpu_full || mem_full {
                        tracker.full_samples += 1;
                    }
                    if m.mem_pressure.is_some_and(|p| p > 20.0) {
                        tracker.pressure_samples += 1;
                    }
                }
            }
        }
        std::thread::sleep(POLL);
    };
    let ended = db::now();
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    let wall = ended - started;
    // The kernel's totals cover the child and every descendant it waited for.
    // Max RSS is bytes on macOS and kilobytes on Linux.
    let ru_cpu_ns = (ru.ru_utime.tv_sec as u64 * 1_000_000_000 + ru.ru_utime.tv_usec as u64 * 1000)
        + (ru.ru_stime.tv_sec as u64 * 1_000_000_000 + ru.ru_stime.tv_usec as u64 * 1000);
    let ru_maxrss_kb = if cfg!(target_os = "macos") { ru.ru_maxrss as u64 / 1024 } else { ru.ru_maxrss as u64 };
    // A final reading for the time since the last sample, from the kernel's
    // totals, so the history holds the whole run. Without it a job shorter
    // than one interval never shows up in the dashboard's load chart.
    let tail = ended - last_sample;
    if tail > 0.05
        && let Some(d) = &database
    {
        // The kernel's total also holds work of short-lived children that the
        // samples missed earlier in the run; it cannot exceed every core.
        let cpu_left = (ru_cpu_ns.saturating_sub(tracker.cpu_ns) as f64 / 1e9).min(sys::ncpu() as f64 * tail);
        let mem = if last_mem_kb > 0 { last_mem_kb } else { ru_maxrss_kb };
        let _ = d.job_sample(run_id, ended, tail, cpu_left / tail, cpu_left / tail, mem, 0.0);
    }
    tracker.cpu_ns = tracker.cpu_ns.max(ru_cpu_ns);
    tracker.peak_mem_kb = tracker.peak_mem_kb.max(ru_maxrss_kb);
    let (used, wanted) = tracker.sustained(wall);
    let measured = tracker.peak_mem_kb > 0;

    let median = database.as_ref().and_then(|d| d.median_duration(&jkey, cfg.hist_keep, run_id).ok().flatten());
    let n = tracker.samples.max(1) as f64;
    let starved = if measured {
        insight::starvation(&RunFacts {
            wall,
            cpu_s: tracker.cpu_ns as f64 / 1e9,
            runnable_s: tracker.runnable_ns as f64 / 1e9,
            used,
            wanted,
            pageins_per_s: tracker.pageins_per_s(wall),
            pressure_frac: tracker.pressure_samples as f64 / n,
            full_frac: tracker.full_samples as f64 / n,
            median_dur: median,
        })
    } else {
        None
    };
    if let Some(d) = &database {
        let _ = d.finish_run(
            run_id,
            &RunResult {
                ended_at: ended,
                exit: code,
                peak_mem_kb: tracker.peak_mem_kb,
                cores_used: used,
                cores_wanted: wanted,
                cpu_seconds: tracker.cpu_ns as f64 / 1e9,
                runnable_seconds: tracker.runnable_ns as f64 / 1e9,
                pageins: tracker.pageins,
                starved: starved.as_ref().map(|s| s.kind.to_string()),
                starved_detail: starved.as_ref().map(|s| serde_json::to_string(&s.evidence).unwrap_or_default()),
                machine_full_frac: tracker.full_samples as f64 / n,
                measured,
            },
        );
    }
    cleanup_files(&q, &me);
    let still = q.waiting().len();
    lines.done(&report::Done { key: &jkey, secs: wall, used, wanted, peak_kb: tracker.peak_mem_kb, code, queued: still });

    if let (Some(st), Some(d)) = (&starved, &database) {
        after_starved(
            d,
            &cfg,
            &o,
            &me,
            &cwd,
            st,
            &learned,
            used,
            wanted,
            tracker.peak_mem_kb,
            script.as_deref(),
            package_json.as_deref(),
            &lines,
        );
    }
    Ok(code)
}

#[allow(clippy::too_many_arguments)]
fn after_starved(
    d: &Db,
    cfg: &Config,
    o: &Opts,
    me: &Entry,
    cwd: &Path,
    st: &insight::Starvation,
    before: &db::Learned,
    used: f64,
    wanted: f64,
    peak: u64,
    script: Option<&str>,
    package_json: Option<&str>,
    lines: &Lines,
) {
    let after = d.learned(&me.key, cfg.hist_keep, cfg.boost_runs).unwrap_or_default();
    let reason = st.evidence.join("; ");
    match st.kind {
        "memory" => {
            let old = before.mem_kb.map(|m| m as f64);
            let new = after.mem_kb.map(|m| m as f64 * 1.25);
            let _ = d.adjustment(&me.key, me.run_id, "mem_boost", old, new, &reason);
        }
        _ => {
            let _ = d.adjustment(&me.key, me.run_id, "cpu_need", before.cpu, after.cpu, &reason);
        }
    }
    let streak = d.starved_streak(&me.key).unwrap_or(0);
    if streak >= 3 {
        let v = if st.kind == "memory" { peak as f64 * 1.25 } else { wanted.ceil() };
        let _ = d.adjustment(&me.key, me.run_id, "suggest_min", None, Some(v), &format!("starved {streak} runs in a row"));
    }
    let what = if st.kind == "cpu" {
        let now_need = after.cpu.unwrap_or(wanted);
        match before.cpu {
            Some(b) if now_need <= b + 0.05 => format!("next run keeps reserving {b:.1} cores"),
            Some(b) => format!("next run will reserve {now_need:.1} cores (was {b:.1})"),
            None => format!("next run will reserve {now_need:.1} cores"),
        }
    } else if st.kind == "memory" {
        format!("next run will reserve {} of memory", report::gb(after.mem_kb.map(|m| (m as f64 * 1.25) as u64).unwrap_or(peak)))
    } else {
        "its needs are raised for the next run".into()
    };
    say(&format!("warning {} - possibly starved: {}; {what}", me.key, st.evidence.join("; ")));
    if !lines.hints {
        return;
    }
    let effective = matcher::effective(&o.cmd);
    let others = others_now(d, me);
    let adv = insight::advice(&AdviceInput {
        argv: &o.cmd,
        effective: &effective,
        starved: st,
        script,
        package_json,
        cwd: &cwd.to_string_lossy(),
        cpu_before: before.cpu,
        cpu_after: after.cpu,
        mem_before: before.mem_kb,
        mem_after: after.mem_kb.map(|m| (m as f64 * 1.25) as u64),
        got_cores: used,
        wanted_cores: wanted,
        peak_mem_kb: peak,
        others,
        streak,
    });
    for a in adv {
        say(&format!("advice {a}"));
    }
}

/// Who else held the room during this run: --now jobs, and the biggest
/// processes outside taskguard.
fn others_now(d: &Db, me: &Entry) -> Vec<String> {
    let mut out = Vec::new();
    let now_jobs: i64 = d
        .conn
        .query_row(
            "SELECT count(*) FROM runs WHERE now = 1 AND id != ?1 AND started_at <= ?2 AND coalesce(ended_at, ?2) >= ?3",
            rusqlite::params![me.run_id, db::now(), me.started_at.unwrap_or(0.0)],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if now_jobs > 0 {
        out.push(format!("{now_jobs} job(s) started with --now"));
    }
    if let Ok(top) = d.latest_top_procs()
        && let Some((name, mem, cores)) = top.first()
    {
        out.push(format!("{name} outside taskguard held {} and {cores:.1} cores", report::gb(*mem)));
    }
    out
}

/// `--bg`: start a detached copy of this job, wait until it is admitted, and
/// return. The copy owns the slot, measures and records the run, and releases
/// the slot when the command ends. Its output still goes to this terminal.
fn run_bg(o: &Opts) -> Result<i32> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!("cannot create a pipe for --bg");
    }
    let (rfd, wfd) = (fds[0], fds[1]);
    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = Vec::new();
    if let Some(j) = &o.jobs {
        args.extend(["-j".into(), j.clone()]);
    }
    if let Some(v) = &o.id {
        args.extend(["--id".into(), v.clone()]);
    }
    if let Some(v) = &o.ns {
        args.extend(["--ns".into(), v.clone()]);
    }
    if let Some(v) = o.timeout {
        args.extend(["--st".into(), v.to_string()]);
    }
    if let Some(v) = o.timeout_exit {
        args.extend(["--st-exit".into(), v.to_string()]);
    }
    if let Some(v) = &o.key {
        args.extend(["--key".into(), v.clone()]);
    }
    if let Some(v) = o.min_cpu {
        args.extend(["--min-cpu".into(), v.to_string()]);
    }
    if let Some(v) = o.min_mem_kb {
        args.extend(["--min-mem".into(), format!("{v}K")]);
    }
    if o.now {
        args.push("--now".into());
    }
    if o.quiet {
        args.push("-q".into());
    }
    match o.hints {
        Some(true) => args.push("--hints".into()),
        Some(false) => args.push("--no-hints".into()),
        None => {}
    }
    args.push("--".into());
    args.extend(o.cmd.iter().cloned());
    let mut cmd = Command::new(exe);
    cmd.arg("run").args(&args).env("TASKGUARD_BG_FD", wfd.to_string());
    if !o.pipe {
        cmd.stdin(Stdio::null());
    }
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            libc::close(rfd);
            Ok(())
        });
    }
    cmd.spawn().context("starting the --bg job")?;
    unsafe { libc::close(wfd) };
    let mut f = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(rfd) };
    let mut buf = [0u8; 1];
    let n = f.read(&mut buf).unwrap_or(0);
    Ok(if n == 1 { 0 } else { 1 })
}

/// `--wait`: block until no job of the pool is running or waiting.
pub fn wait_pool(id: Option<&str>) -> Result<i32> {
    let q = Queue::open(&config::state_dir())?;
    let id = id.unwrap_or("default");
    loop {
        {
            let _g = q.lock()?;
            q.reap();
        }
        let busy = q.running().into_iter().chain(q.waiting()).any(|e| e.pool.as_deref().unwrap_or("default") == id);
        if !busy {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_like_sem() {
        assert_eq!(parse_jobs("4", 12).unwrap(), 4);
        assert_eq!(parse_jobs("+0", 12).unwrap(), 12);
        assert_eq!(parse_jobs("+2", 12).unwrap(), 14);
        assert_eq!(parse_jobs("-2", 12).unwrap(), 10);
        assert_eq!(parse_jobs("-20", 12).unwrap(), 1);
        assert_eq!(parse_jobs("50%", 12).unwrap(), 6);
        assert!(parse_jobs("lots", 12).is_err());
    }
}
