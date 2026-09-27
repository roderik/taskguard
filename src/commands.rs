//! The one-shot subcommands: status, history, doctor, import-history.

use crate::config::{self, Config};
use crate::dash;
use crate::db::{self, Db};
use crate::key;
use crate::machine;
use crate::matcher;
use crate::queue::{Limits, Queue};
use crate::report::{self, Snapshot, dur, gb};
use crate::sys;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn limits(cfg: &Config) -> Limits {
    Limits {
        cpu_max_pct: cfg.cpu_max,
        mem_max_pct: cfg.mem_max,
        learn_stagger: cfg.learn_stagger,
        max_bypass: cfg.max_bypass as f64,
        max_backfill: cfg.max_backfill as f64,
        cpu_min_duration: cfg.cpu_min_duration,
        pressure_max: cfg.pressure_max,
    }
}

/// Everything the status screen and the dashboard's Queue view show.
pub fn snapshot(dir: &Path, cfg: &Config, db: Option<&Db>) -> Result<Snapshot> {
    let q = Queue::open(dir)?;
    let (m, running, waiting, unknown_starts) = {
        let _g = q.lock()?;
        for gone in q.reap() {
            if let Some(d) = db {
                let _ = d.abandon_run(gone);
            }
        }
        let running = q.running();
        let ours = running.iter().map(|e| e.live_mem_kb).sum();
        (machine::current(dir, 2.0, ours), running, q.waiting(), q.unknown_starts())
    };
    let mut s = Snapshot::build(m, limits(cfg), running, waiting, db::now(), &unknown_starts);
    if let Some(d) = db {
        let since = db::now() - 24.0 * 3600.0;
        if let Ok(w) = dash::warnings(d, since, cfg.cpu_max, cfg.mem_max) {
            s.warnings = w.iter().map(|w| format!("{} - {}", w.title, w.lines.first().cloned().unwrap_or_default())).collect();
            s.advice = w.iter().filter(|w| w.kind == "repeated").flat_map(|w| w.lines.iter().skip(1).cloned()).collect();
        }
        s.top_other = d.latest_top_procs().unwrap_or_default();
    }
    Ok(s)
}

pub fn status(json: bool) -> Result<i32> {
    let dir = config::state_dir();
    let cwd = std::env::current_dir()?;
    let cfg = Config::load(&cwd, &key::checkout_root(&cwd))?;
    let db = Db::open_dir(&dir).ok();
    let s = snapshot(&dir, &cfg, db.as_ref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        print!("{}", report::render_status(&s));
    }
    Ok(0)
}

pub fn history() -> Result<i32> {
    let dir = config::state_dir();
    let db = Db::open_dir(&dir)?;
    let cfg = Config::load(Path::new("/"), Path::new("/")).unwrap_or_default();
    let rows = dash::history(&db, cfg.hist_keep)?;
    if rows.is_empty() {
        println!("no runs recorded yet");
        return Ok(0);
    }
    println!("{:<52} {:<14} {:>5} {:>7} {:>9} {:>8} {:>5}", "key", "namespace", "runs", "cores", "memory", "last", "exit");
    for r in rows {
        println!(
            "{:<52} {:<14} {:>5} {:>7} {:>9} {:>8} {:>5}",
            r.key,
            r.ns,
            r.runs,
            r.cpu.map(|c| format!("{c:.1}")).unwrap_or_else(|| "-".into()),
            r.mem_kb.map(gb).unwrap_or_else(|| "-".into()),
            r.last_dur_s.map(dur).unwrap_or_else(|| "-".into()),
            r.last_exit.map(|e| e.to_string()).unwrap_or_else(|| "-".into()),
        );
    }
    Ok(0)
}

pub fn doctor(args: &[String]) -> Result<i32> {
    let dir = config::state_dir();
    let cwd = std::env::current_dir()?;
    let checkout = key::checkout_root(&cwd);
    let cfg = Config::load(&cwd, &checkout)?;

    if let Some(i) = args.iter().position(|a| a == "--explain") {
        let Some(cmdline) = args.get(i + 1) else {
            anyhow::bail!("--explain needs a command, in quotes");
        };
        return explain(cmdline, &cwd, &cfg, &dir);
    }

    println!("taskguard {}", env!("CARGO_PKG_VERSION"));
    println!("state dir:      {}", dir.display());
    println!(
        "user config:    {}{}",
        config::user_config_path().display(),
        if config::user_config_path().exists() { "" } else { " (not present; built-in defaults)" }
    );
    println!("layers here:    {}", cfg.layers.join("  <  "));
    println!("namespace here: {}", key::namespace(&cwd));
    println!();
    println!(
        "limits: cpu_max {:.0}%  mem_max {:.0}%  learn_stagger {}s  max_bypass {}s  max_backfill {}s  hints {}",
        cfg.cpu_max, cfg.mem_max, cfg.learn_stagger, cfg.max_bypass, cfg.max_backfill, cfg.hints
    );
    for (k, v) in &cfg.origin {
        if v != "built-in" {
            println!("  {k} set by {v}");
        }
    }
    println!();
    let m = machine::measure(None, 0);
    println!(
        "machine: {} cores, {:.1} busy; memory {} of {} held ({:.0}%){}",
        m.ncpu,
        m.cpu_busy,
        gb(m.mem_used_kb),
        gb(m.mem_total_kb),
        m.mem_pct(),
        m.mem_pressure.map(|p| format!(", pressure {p:.0}")).unwrap_or_default()
    );
    let me = sys::proc_sample(std::process::id() as i32);
    println!(
        "per-process readings: {}",
        match me {
            Some(s) => format!("ok (this process: {}, {:.3}s CPU)", gb(s.footprint_kb), s.cpu_ns as f64 / 1e9),
            None => "NOT AVAILABLE".into(),
        }
    );
    let rec_running = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("recorder.lock"))
        .map(|f| f.try_lock().is_err())
        .unwrap_or(false);
    println!("recorder: {}", if rec_running { "running" } else { "not running (it starts with the next job)" });
    println!();
    println!(
        "pools: {}",
        cfg.pools
            .iter()
            .map(|p| format!(
                "{} ({} slot{})",
                p.name,
                p.max_slots.map(|s| s.to_string()).unwrap_or("no limit".into()),
                if p.max_slots == Some(1) { "" } else { "s" }
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("passthrough patterns: {}", cfg.passthrough.len());
    println!("[[job]] rules: {}", cfg.jobs.len());

    let shims = tsc_queue_leftovers();
    if !shims.is_empty() {
        println!();
        println!("tsc-queue shims are still installed in {} place(s), for example:", shims.len());
        for s in shims.iter().take(5) {
            println!("  {}", s.display());
        }
        println!("remove them with the old tool before relying on taskguard:");
        println!("  tsc-queue unload && tsc-queue uninstall");
    }
    Ok(0)
}

fn explain(cmdline: &str, cwd: &Path, cfg: &Config, dir: &Path) -> Result<i32> {
    let argv = matcher::split_shell(cmdline);
    let key = key::run_key(cwd, &argv);
    let cls = cfg.classify(&argv, &key)?;
    println!("command:    {cmdline}");
    println!("effective:  {}", matcher::effective(&argv).join(" "));
    println!("key:        {key}");
    println!("namespace:  {}", key::namespace(cwd));
    println!("label:      {}", cls.label.as_deref().unwrap_or("-"));
    println!(
        "queued:     {}",
        if cls.passthrough {
            "no - passthrough, it runs straight through unmeasured"
        } else if cls.now {
            "no - now = true, it skips the queue but is measured"
        } else {
            "yes"
        }
    );
    match &cls.pool {
        Some((n, s, pc)) => println!(
            "pool:       {n} ({}{})",
            s.map(|s| format!("{s} slot(s)")).unwrap_or("no slot limit".into()),
            if *pc { ", one per checkout" } else { "" }
        ),
        None => println!("pool:       none"),
    }
    for w in &cls.why {
        println!("  because {w}");
    }
    if let Ok(db) = Db::open_dir(dir) {
        let l = db.learned(&key, cfg.hist_keep, cfg.boost_runs)?;
        if l.runs == 0 {
            println!("learned:    nothing yet; the first run just runs");
        } else {
            println!(
                "learned:    {} runs; {} cores, {} memory{}, usually {}",
                l.runs,
                l.cpu.map(|c| format!("{c:.1}")).unwrap_or("?".into()),
                l.mem_kb.map(gb).unwrap_or("?".into()),
                if l.mem_boosted { " (+25% after a memory-starved run)" } else { "" },
                l.dur_s.map(dur).unwrap_or("?".into())
            );
        }
    }
    Ok(0)
}

/// Compiler paths tsc-queue wrapped, in the checkouts its old config named.
fn tsc_queue_leftovers() -> Vec<PathBuf> {
    let conf = config::home().join(".config/tsc-queue.conf");
    let Ok(text) = std::fs::read_to_string(&conf) else { return Vec::new() };
    let var = |name: &str| -> Vec<PathBuf> {
        text.lines()
            .filter_map(|l| l.trim().strip_prefix(&format!("{name}=")))
            .flat_map(|v| {
                v.split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .replace("$HOME", &config::home().to_string_lossy())
                    .split_whitespace()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let mut checkouts = var("TSC_QUEUE_ROOTS");
    for s in var("TSC_QUEUE_SCAN") {
        if let Ok(rd) = std::fs::read_dir(&s) {
            checkouts.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    let mut out = Vec::new();
    for c in checkouts {
        {
            let rel = "node_modules/typescript/bin/tsc";
            let p = c.join(rel);
            if std::fs::read(&p).map(|b| String::from_utf8_lossy(&b[..b.len().min(200)]).contains("tsc-queue")).unwrap_or(false) {
                out.push(p);
            }
        }
        for store in [".pnpm", ".bun"] {
            if let Ok(rd) = std::fs::read_dir(c.join("node_modules").join(store)) {
                for e in rd.filter_map(|e| e.ok()) {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let cands: Vec<PathBuf> = if n.starts_with("typescript@") {
                        vec![e.path().join("node_modules/typescript/bin/tsc")]
                    } else if n.starts_with("@typescript+typescript-darwin") {
                        std::fs::read_dir(e.path().join("node_modules/@typescript"))
                            .map(|rd| rd.filter_map(|x| x.ok()).map(|x| x.path().join("lib/tsc")).collect())
                            .unwrap_or_default()
                    } else {
                        vec![]
                    };
                    for p in cands {
                        if std::fs::read(&p).map(|b| String::from_utf8_lossy(&b[..b.len().min(200)]).contains("tsc-queue")).unwrap_or(false)
                        {
                            out.push(p);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Loads tsc-queue's history. Its keys were the project path with `/` turned
/// into `_`, with no command, so they are stored under `tsc-queue:<key>` and
/// used for a tsc or tsgo job that has no history of its own yet.
pub fn import_history() -> Result<i32> {
    let src =
        std::env::var_os("TSC_QUEUE_DIR").map(PathBuf::from).unwrap_or_else(|| config::home().join(".cache/tsc-queue")).join("history.tsv");
    let text = std::fs::read_to_string(&src).map_err(|e| anyhow::anyhow!("reading {}: {e}", src.display()))?;
    let db = Db::open_dir(&config::state_dir())?;
    let done: i64 = db.conn.query_row("SELECT count(*) FROM runs WHERE imported = 1", [], |r| r.get(0))?;
    if done > 0 {
        println!("already imported ({done} runs); nothing to do");
        return Ok(0);
    }
    let tx = db.conn.unchecked_transaction()?;
    let mut n = 0;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 {
            continue;
        }
        let (Ok(ts), Ok(peak), Ok(secs), Ok(exit)) = (f[0].parse::<f64>(), f[2].parse::<i64>(), f[3].parse::<f64>(), f[4].parse::<i64>())
        else {
            continue;
        };
        if peak <= 0 {
            continue;
        }
        tx.execute(
            "INSERT INTO runs (ns, key, cmd, queued_at, started_at, ended_at, exit, peak_mem_kb, imported, label)
             VALUES ('imported', ?1, 'tsc', ?2, ?2, ?3, ?4, ?5, 1, 'typecheck')",
            rusqlite::params![format!("tsc-queue:{}", f[1]), ts - secs, ts, exit, peak],
        )?;
        n += 1;
    }
    tx.commit()?;
    println!("imported {n} runs from {}", src.display());
    Ok(0)
}

/// The tsc-queue key for a cwd: the project path with `/` and spaces as `_`.
pub fn legacy_key(cwd: &Path) -> String {
    key::project_path(cwd).replace(['/', ' '], "_")
}
