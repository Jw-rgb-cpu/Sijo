//! `sijoc live`: live coding. The game keeps running while you edit the source.
//!
//! On every save the file is recompiled. If it compiles, the running game is asked to hand over
//! (it writes its world state and exits), and the new build continues from the same tick with
//! the state migrated by field name: new fields get their defaults, removed fields are dropped,
//! entity handles stay valid. If it does not compile, the errors are shown and the old build
//! keeps running.

use crate::diag::{Diags, Level};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime};

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

struct Builder {
    inputs: Vec<String>,
    src_path: PathBuf,
    dir: PathBuf,
    cc: String,
    generation: u32,
}

impl Builder {
    /// Compile the current source; returns the executable on success.
    fn build(&mut self) -> Option<PathBuf> {
        let t0 = Instant::now();
        let sm = match crate::driver::load_project(&self.inputs) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[live] {}", e);
                return None;
            }
        };
        let src = sm.text.clone();
        let mut d = Diags::default();
        let shown = self.src_path.display().to_string();
        let Some(c) = crate::driver::compile_with(&src, &mut d, &crate::driver::Options { dev: true, order: crate::driver::project_config(&self.inputs).map(|c| c.order).unwrap_or_default(), lib: false }) else {
            for x in &d.list {
                eprint!("{}", sm.render(x));
            }
            eprintln!("[live] {} error(s): the running game keeps the previous build", d.list.iter().filter(|x| x.level == Level::Error).count());
            return None;
        };
        for x in d.list.iter().filter(|x| x.level != Level::Hint) {
            eprint!("{}", sm.render(x));
        }
        // the same .sijolint handling as every other command (its messages are shown; a broken file falls back to the defaults so the game keeps running)
        let cfg = crate::driver::load_lint_config(&self.src_path.to_string_lossy()).unwrap_or_default();
        let lints = crate::lint::lint(&c, &src, &cfg);
        let warnings = lints.iter().filter(|l| l.level == Level::Warning).count();
        self.generation += 1;
        let cfile = self.dir.join(format!("v{}.c", self.generation));
        let exe = self.dir.join(format!("v{}", self.generation));
        let cname = cfile.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if std::fs::write(&cfile, crate::codegen::generate_with(&c, &shown, &sm, &cname)).is_err() {
            eprintln!("[live] cannot write {}", cfile.display());
            return None;
        }
        // -O1 keeps the edit -> running latency short
        let out = Command::new(&self.cc)
            .args(["-std=gnu11", "-O1", "-ffp-contract=off", "-w"])
            .arg(&cfile)
            .arg("-o")
            .arg(&exe)
            .arg("-lm")
            .output();
        match out {
            Ok(o) if o.status.success() => {
                eprintln!(
                    "[live] build v{} ready in {} ms{}",
                    self.generation,
                    t0.elapsed().as_millis(),
                    if warnings > 0 { format!(" ({} lint warning(s): sijoc lint)", warnings) } else { String::new() }
                );
                Some(exe)
            }
            Ok(o) => {
                eprintln!("[live] C compilation failed:\n{}", String::from_utf8_lossy(&o.stderr));
                None
            }
            Err(e) => {
                eprintln!("[live] cannot run {}: {}", self.cc, e);
                None
            }
        }
    }
}

fn spawn(exe: &Path, dir: &Path, resume: bool, user_args: &[String]) -> Option<Child> {
    let mut cmd = Command::new(exe);
    cmd.arg("--live-dir").arg(dir).arg("--ticks").arg("0").arg("--no-summary");
    if !user_args.iter().any(|a| a == "--fast") {
        cmd.arg("--realtime");
    }
    if resume {
        cmd.arg("--load-state").arg(dir.join("state"));
    }
    cmd.args(user_args.iter().filter(|a| *a != "--fast"));
    match cmd.spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[live] cannot start the game: {}", e);
            None
        }
    }
}

/// Ask the running game to save its state and exit; returns true if it handed over.
fn hand_over(child: &mut Child, dir: &Path) -> bool {
    let _ = std::fs::write(dir.join("reload"), b"1");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return status.code() == Some(42);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// newest modification time over the project's files (and the file list itself)
fn project_stamp(inputs: &[String]) -> (Vec<PathBuf>, Option<SystemTime>) {
    let files = crate::driver::project_files(inputs);
    let newest = files.iter().filter_map(|f| mtime(f)).max();
    (files, newest)
}

pub fn run(inputs: &[String], args: &[String]) -> i32 {
    let path = inputs.join(" ");
    let src_path = PathBuf::from(&inputs[0]);
    let user_args: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => args.to_vec(),
    };
    let max_reloads: Option<u32> = std::env::var("SIJO_LIVE_MAX_RELOADS").ok().and_then(|v| v.parse().ok());
    let dir = std::env::temp_dir().join(format!("sijo-live-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::remove_file(dir.join("reload"));
    let mut b = Builder { inputs: inputs.to_vec(), src_path: src_path.clone(), dir: dir.clone(), cc: std::env::var("CC").unwrap_or_else(|_| "gcc".into()), generation: 0 };
    eprintln!("[live] watching {} — edit and save; Ctrl+C to stop", path);
    let mut child: Option<Child> = None;
    let mut have_state = false;
    if let Some(exe) = b.build() {
        child = spawn(&exe, &dir, false, &user_args);
    }
    let mut last = project_stamp(inputs);
    let mut reloads = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(c) = child.as_mut() {
            if let Ok(Some(status)) = c.try_wait() {
                eprintln!("[live] game exited ({}); waiting for changes", status);
                child = None;
            }
        }
        let now = project_stamp(inputs);
        if now == last {
            continue;
        }
        last = now;
        std::thread::sleep(Duration::from_millis(50)); // let the editor finish writing
        let t0 = Instant::now();
        let Some(exe) = b.build() else { continue };
        if let Some(mut c) = child.take() {
            have_state = hand_over(&mut c, &dir);
            if !have_state {
                eprintln!("[live] the old build did not hand over its state; starting fresh");
            }
        }
        child = spawn(&exe, &dir, have_state, &user_args);
        reloads += 1;
        eprintln!("[live] reloaded (v{}) in {} ms{}", b.generation, t0.elapsed().as_millis(), if have_state { ", state kept" } else { "" });
        if max_reloads.map(|m| reloads >= m).unwrap_or(false) {
            // used by tests: stop after N reloads, giving the new build a moment to run
            std::thread::sleep(Duration::from_millis(600));
            if let Some(mut c) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            return 0;
        }
    }
}
