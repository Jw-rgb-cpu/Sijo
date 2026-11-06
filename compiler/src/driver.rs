//! Command-line driver: sijoc check|explain|build|run <file.sijo>

use crate::diag::{Diags, Level, SourceMap};
use crate::{ecs, effects, memory, net, opt, parser, safety, sema};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Compiled {
    /// per function: locals read in the source as written
    pub reads_before_opt: Vec<std::collections::HashSet<crate::hir::LocalId>>,
    /// integer divisions whose result is converted to float (before constant folding)
    pub int_div_float: Vec<crate::diag::Span>,
    pub ast: crate::ast::File,
    pub prog: crate::hir::Program,
    pub info: effects::EffectInfo,
    pub ecs: ecs::EcsPlan,
    pub mem: memory::MemPlan,
    pub net: net::NetPlan,
    pub opt: opt::OptStats,
    pub spatial: crate::spatial::SpatialPlan,
}

/// Runs the whole front/middle end. Returns None if there were errors.
/// Deeply nested programs recurse deeply in every pass, so this runs on a dedicated large stack.
pub fn compile(src: &str, diags: &mut Diags) -> Option<Compiled> {
    compile_with(src, diags, &Options::default())
}

/// Compiler options. `dev` (used by live coding) keeps every field's storage so that state
/// survives reloads even when a later edit starts reading a field that was unused before.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub dev: bool,
}

/// Optimizer switches that never change what a program computes (only how fast it runs).
#[derive(Clone, Debug, Default)]
pub struct Tuning {
    /// skip equality saturation (`--no-egraph`: to compare against, or to rule it out)
    pub no_egraph: bool,
    /// an execution profile (`--profile-use FILE`, written by a program run with `--profile FILE`)
    pub profile: Option<crate::profile::Profile>,
    /// `--open-tables`: every class that can gain components keeps one open table
    pub open_tables: bool,
}

pub fn compile_with(src: &str, diags: &mut Diags, opts: &Options) -> Option<Compiled> {
    compile_tuned(src, diags, opts, &Tuning::default())
}

pub fn compile_tuned(src: &str, diags: &mut Diags, opts: &Options, tune: &Tuning) -> Option<Compiled> {
    let first = diags.list.len();
    let out = std::thread::scope(|sc| {
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn_scoped(sc, || compile_here(src, diags, opts, tune))
            .expect("cannot spawn compiler thread")
            .join()
            .unwrap_or(None)
    });
    // the instances of a generic method or class share their source: a finding about that source
    // (the same code, place and message) is reported once
    let mut i = first;
    while i < diags.list.len() {
        let x = &diags.list[i];
        if diags.list[first..i].iter().any(|y| y.level == x.level && y.code == x.code && y.span == x.span && y.msg == x.msg) {
            diags.list.remove(i);
        } else {
            i += 1;
        }
    }
    out
}

fn compile_here(src: &str, diags: &mut Diags, opts: &Options, tune: &Tuning) -> Option<Compiled> {
    let file = parser::parse(src, diags);
    if diags.has_errors() {
        return None;
    }
    let mut prog = sema::check(&file, diags);
    if diags.has_errors() {
        return None;
    }
    // [Recursive] marks the program as written: the calls before optimization folds any away
    let call_edges = sema::call_edges(&prog);
    // facts the linter needs from the program as written (before constant propagation)
    let reads_before_opt = crate::lint::local_reads(&prog);
    let int_div_float = crate::lint::int_divisions_as_float(&prog);
    let opt = opt::optimize_with(&mut prog, !tune.no_egraph);
    // after constant propagation `x / Z` with `const int Z = 0` is `x / 0`: C# rejects it (CS0020)
    for f in &prog.funcs {
        crate::sema::visit_block(&f.body, &mut |e| {
            use crate::ast::BinOp::{Div, Rem};
            use crate::hir::ExprKind as K;
            let zero = match &e.kind {
                K::Binary(Div | Rem, _, b) if e.ty == crate::hir::Ty::Int => Some(b),
                K::Assign { op: Some(Div | Rem), target, value } if target.ty == crate::hir::Ty::Int => Some(value),
                _ => None,
            };
            if let Some(b) = zero {
                if matches!(b.kind, K::Int(0)) {
                    diags.error("E0256", b.span, "division by constant zero");
                }
            }
        });
    }
    if diags.has_errors() {
        return None;
    }
    // the component combinations each class can reach become archetypes (docs/04 §4.4.1)
    crate::components::expand(&mut prog, diags, tune.open_tables);
    if diags.has_errors() {
        return None;
    }
    let analyzed = diags.list.len();
    let info = effects::analyze(&mut prog, diags);
    sema::check_recursive(&prog, &call_edges, diags);
    safety::check(&prog, &info, diags);
    // a struct's copy of an interface's default implementation is checked like the default itself
    diags.dedupe_from(analyzed);
    if diags.has_errors() {
        return None;
    }
    let ecs = ecs::plan_profiled(&prog, &info, diags, opts.dev, &opt.observed_fields, tune.profile.as_ref());
    let mem = memory::plan(&prog);
    let net = net::plan(&prog, &ecs, diags);
    if diags.has_errors() {
        return None;
    }
    let spatial = crate::spatial::plan(&prog, &ecs);
    Some(Compiled { ast: file, prog, info, ecs, mem, net, opt, reads_before_opt, int_div_float, spatial })
}

// ---------------------------------------------------------------------------
// projects: one file, several files, a directory, or a directory with sijo.toml
// ---------------------------------------------------------------------------

/// nearest directory at or above `file`'s directory that contains `sijo.toml`
pub fn project_root(file: &Path) -> Option<PathBuf> {
    let start = if file.is_dir() { file.to_path_buf() } else { file.parent().map(|p| p.to_path_buf()).unwrap_or_default() };
    let start = if start.as_os_str().is_empty() { PathBuf::from(".") } else { start };
    let abs = std::fs::canonicalize(&start).ok()?;
    abs.ancestors().find(|d| d.join("sijo.toml").is_file()).map(|d| d.to_path_buf())
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with('.') || name == "target" || name == "build" {
            continue;
        }
        if p.is_dir() {
            collect_sources(&p, out);
        } else if p.extension().map(|e| e == "sijo").unwrap_or(false) {
            out.push(p);
        }
    }
}

/// The source files of a project, in a stable order.
/// - a directory: every `.sijo` file below it
/// - a file inside a directory tree with `sijo.toml`: every `.sijo` file of that project
/// - otherwise exactly the files given
pub fn project_files(inputs: &[String]) -> Vec<PathBuf> {
    let mut files = vec![];
    if inputs.len() == 1 {
        let p = Path::new(&inputs[0]);
        if p.is_dir() {
            collect_sources(p, &mut files);
        } else if let Some(root) = project_root(p) {
            collect_sources(&root, &mut files);
            // show paths relative to the current directory when possible
            if let Ok(cwd) = std::env::current_dir() {
                files = files.into_iter().map(|f| f.strip_prefix(&cwd).map(|r| r.to_path_buf()).unwrap_or(f)).collect();
            }
        } else {
            files.push(p.to_path_buf());
        }
    } else {
        for i in inputs {
            let p = Path::new(i);
            if p.is_dir() {
                collect_sources(p, &mut files);
            } else {
                files.push(p.to_path_buf());
            }
        }
    }
    files.dedup();
    files
}

pub fn load_project(inputs: &[String]) -> Result<SourceMap, String> {
    let files = project_files(inputs);
    if files.is_empty() {
        return Err(format!("no .sijo files found in {}", inputs.join(" ")));
    }
    let mut v = vec![];
    for f in files {
        let src = std::fs::read_to_string(&f).map_err(|e| format!("cannot read {}: {}", f.display(), e))?;
        v.push((f.display().to_string(), src));
    }
    Ok(SourceMap::new(v))
}

pub fn print_diags(sm: &SourceMap, d: &Diags) {
    for x in &d.list {
        eprint!("{}", sm.render(x));
    }
    let e = d.list.iter().filter(|x| x.level == Level::Error).count();
    let w = d.list.len() - e;
    if e > 0 || w > 0 {
        eprintln!("{} error(s), {} warning(s)", e, w);
    }
}

fn usage() -> i32 {
    eprintln!(
        "sijoc {} — Sijo game language compiler (prototype)

<input> is a .sijo file, several files, or a project directory (all .sijo files below it;
a file inside a directory tree with sijo.toml means the whole project)

usage:
  sijoc check   <input>                     type-check, run all safety analyses and the linter
  sijoc lint    <file.sijo> [--fix] [--preset standard|dotnet] [--deny-warnings]
  sijoc lint    --list                      list all lint rules and their sources
  sijoc live    <file.sijo> [-- <program args>]   live coding: edit while the game runs
  sijoc lsp                                 language server (IntelliSense) on stdin/stdout
  sijoc explain <file.sijo>                 show ECS / schedule / memory / network decisions
  sijoc build   <input> [-o out.c] [--exe path] [--cc gcc] [--no-openmp] [--debug]
                [--lib [--lib-prefix name]] [--host host.c ...] [--externs ext.c ...] [--checked]
                [--cflags \"..\"] [--ldflags \"..\"] [--no-egraph] [--profile-use FILE]
                --lib writes out.c + sijo_game.h (or <name>.h: API functions <name>_create...)
                with an embedding API instead of main; --host compiles your C program (main,
                extern functions) with the game; --externs compiles C files that define the
                extern functions and keeps sijoc's main (loopback, --server, --connect);
                --checked makes int overflow, int.MinValue / -1 and out-of-range float->int panic;
                --no-egraph skips equality saturation (same results, for comparison);
                --profile-use FILE plans with a profile the program wrote when run with
                --profile FILE (parallel thresholds, task groups, hot fields; same results);
                --profile-fields makes that program count every entity field access, so the
                profile also says which fields of a record are busy and which are not;
                --gpu runs the row loops of pure self-write systems as OpenMP target regions
                (a compute device when the C compiler has an offload target; same results)
                --open-tables keeps one table per class with a bit per component (what a
                class with more than 16 combinations gets) for every class; same results
  sijoc run     <input> [--cc gcc] [--no-openmp] [--externs ext.c] [-- <program args>]
  sijoc --version                           print the compiler version

program args: --ticks N  --shards N  --seed S  --render  --quiet  (the built program's --help
lists networking options)",
        env!("CARGO_PKG_VERSION")
    );
    2
}

fn print_lint(sm: &SourceMap, ds: &[crate::lint::LintDiag]) {
    for d in ds {
        eprint!("{}", sm.render(&d.to_diagnostic()));
    }
}

fn lint_summary(ds: &[crate::lint::LintDiag]) -> String {
    let e = ds.iter().filter(|d| d.level == Level::Error).count();
    let w = ds.iter().filter(|d| d.level == Level::Warning).count();
    let h = ds.len() - e - w;
    let fixable = ds.iter().filter(|d| d.fix.is_some()).count();
    let errs = if e > 0 { format!("{} error(s), ", e) } else { String::new() };
    format!("lint: {}{} warning(s), {} hint(s), {} auto-fixable (sijoc lint for details)", errs, w, h, fixable)
}

/// The `.sijolint` that applies to `path`, the same way for every command: an error (unknown rule,
/// bad value) is shown and the command stops (None).
pub fn load_lint_config(path: &str) -> Option<crate::lint::LintConfig> {
    match crate::lint::LintConfig::for_file(Path::new(path)) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("error: {}", e);
            None
        }
    }
}

fn lint_cmd(sm: SourceMap, args: &[String]) -> i32 {
    let path = sm.files[0].path.clone();
    let Some(mut cfg) = load_lint_config(&path) else { return 2 };
    let mut fix = false;
    let mut deny = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--fix" => fix = true,
            "--deny-warnings" => deny = true,
            "--preset" if i + 1 < args.len() => {
                cfg.preset = match args[i + 1].as_str() {
                    "dotnet" | "csharp" => crate::lint::Preset::Dotnet,
                    _ => crate::lint::Preset::Standard,
                };
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let (fixed_text, total_fixed) = if fix { crate::lint::fix_all(&sm.text, &cfg) } else { (sm.text.clone(), 0) };
    let mut sm = sm;
    if total_fixed > 0 {
        let parts = SourceMap::split(&fixed_text);
        if parts.len() != sm.files.len() {
            eprintln!("error: lint --fix changed the file layout; nothing was written");
            return 2;
        }
        let mut files = vec![];
        for (i, new) in parts.into_iter().enumerate() {
            let f = &sm.files[i];
            if sm.file_text(i) != new {
                if let Err(e) = std::fs::write(&f.path, &new) {
                    eprintln!("error: cannot write {}: {}", f.path, e);
                    return 2;
                }
            }
            files.push((f.path.clone(), new));
        }
        sm = SourceMap::new(files);
    }
    let mut d = Diags::default();
    let Some(c) = compile(&sm.text, &mut d) else {
        print_diags(&sm, &d);
        return 1;
    };
    let ds = crate::lint::lint(&c, &sm.text, &cfg);
    print_diags(&sm, &d);
    print_lint(&sm, &ds);
    if total_fixed > 0 {
        let what = if sm.files.len() == 1 { path.clone() } else { format!("{} files", sm.files.len()) };
        eprintln!("applied {} fix(es) to {}", total_fixed, what);
    }
    eprintln!("{}", lint_summary(&ds).replace(" (sijoc lint for details)", ""));
    let bad = ds.iter().any(|d| d.level == Level::Error) || (deny && ds.iter().any(|d| d.level == Level::Warning));
    if bad {
        1
    } else {
        0
    }
}

pub fn main(args: &[String]) -> i32 {
    if args.len() >= 2 && args[1] == "lint" && args.iter().any(|a| a == "--list") {
        println!("{:<6} {:<26} {:<8} {:<8} {}", "code", "name", "standard", "dotnet", "source / description");
        let lv = |l: Option<Level>| match l {
            None => "off",
            Some(Level::Hint) => "hint",
            Some(Level::Warning) => "warning",
            Some(Level::Error) => "error",
        };
        for r in crate::lint::RULES {
            println!("{:<6} {:<26} {:<8} {:<8} {} — {}", r.code, r.name, lv(r.standard), lv(r.dotnet), r.source, r.desc);
        }
        return 0;
    }
    if args.len() >= 2 && args[1] == "lsp" {
        return crate::lsp::serve();
    }
    if args.len() >= 2 && (args[1] == "--version" || args[1] == "-V") {
        println!("sijoc {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        usage();
        return 0;
    }
    if args.len() < 3 {
        return usage();
    }
    let cmd = args[1].as_str();
    // inputs: everything up to the first option
    let n_inputs = args[2..].iter().take_while(|a| !a.starts_with('-')).count().max(1);
    let inputs: Vec<String> = args[2..2 + n_inputs].to_vec();
    let rest = &args[2 + n_inputs..];
    let sm = match load_project(&inputs) {
        Ok(sm) => sm,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    let path = &sm.files[0].path.clone();
    if cmd == "lint" {
        return lint_cmd(sm, rest);
    }
    if cmd == "live" {
        return crate::live::run(&inputs, rest);
    }
    let src = sm.text.clone();
    let mut diags = Diags::default();
    crate::consteval::CHECKED.store(rest.iter().any(|a| a == "--checked"), std::sync::atomic::Ordering::Relaxed);
    crate::consteval::FIELD_COUNTS.store(rest.iter().any(|a| a == "--profile-fields"), std::sync::atomic::Ordering::Relaxed);
    crate::consteval::GPU.store(rest.iter().any(|a| a == "--gpu"), std::sync::atomic::Ordering::Relaxed);
    let mut tune = Tuning { no_egraph: rest.iter().any(|a| a == "--no-egraph"), profile: None, open_tables: rest.iter().any(|a| a == "--open-tables") };
    if let Some(i) = rest.iter().position(|a| a == "--profile-use") {
        let Some(file) = rest.get(i + 1) else {
            eprintln!("error: --profile-use needs a file (written by the program run with --profile FILE)");
            return 2;
        };
        match crate::profile::Profile::read(Path::new(file)) {
            Ok(p) => tune.profile = Some(p),
            Err(e) => {
                eprintln!("error: {}", e);
                return 2;
            }
        }
    }
    let compiled = compile_tuned(&src, &mut diags, &Options::default(), &tune);
    print_diags(&sm, &diags);
    let Some(c) = compiled else { return 1 };
    // options
    let mut out_c: Option<PathBuf> = None;
    let mut exe: Option<PathBuf> = None;
    let mut cc = std::env::var("CC").unwrap_or_else(|_| "gcc".into());
    let mut openmp = true;
    let mut prog_args: Vec<String> = vec![];
    let mut cx = CcOpts::default();
    let mut i = 0;
    let args = rest;
    while i < args.len() {
        match args[i].as_str() {
            "--lib" => cx.lib = true,
            "--checked" | "--no-egraph" | "--profile-fields" | "--open-tables" => {} // handled before compiling
            // the device compiler keeps IEEE single precision without contraction; GCC's offload
            // compilers do not support Ubuntu's default -fcf-protection
            "--gpu" => cx.cflags.extend(["-fcf-protection=none".to_string(), "-foffload-options=-ffp-contract=off -fcf-protection=none".to_string()]),
            "--profile-use" if i + 1 < args.len() => i += 1,
            "--debug" => cx.debug = true,
            "--host" if i + 1 < args.len() => {
                cx.host.push(PathBuf::from(&args[i + 1]));
                cx.lib = true;
                i += 1;
            }
            "--externs" if i + 1 < args.len() => {
                cx.host.push(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--lib-prefix" if i + 1 < args.len() => {
                let p = args[i + 1].clone();
                if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || p.starts_with(|c: char| c.is_ascii_digit()) {
                    eprintln!("error: --lib-prefix must be a C identifier, not '{}'", p);
                    return 2;
                }
                cx.prefix = Some(p);
                i += 1;
            }
            "--ldflags" if i + 1 < args.len() => {
                cx.ldflags.extend(args[i + 1].split_whitespace().map(|s| s.to_string()));
                i += 1;
            }
            "--cflags" if i + 1 < args.len() => {
                cx.cflags.extend(args[i + 1].split_whitespace().map(|s| s.to_string()));
                i += 1;
            }
            "-o" if i + 1 < args.len() => {
                out_c = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--exe" if i + 1 < args.len() => {
                exe = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--cc" if i + 1 < args.len() => {
                cc = args[i + 1].clone();
                i += 1;
            }
            "--no-openmp" => openmp = false,
            "--" => {
                prog_args.extend(args[i + 1..].iter().cloned());
                break;
            }
            other => prog_args.push(other.to_string()),
        }
        i += 1;
    }
    let Some(lint_cfg) = load_lint_config(path) else { return 2 };
    let lints = crate::lint::lint(&c, &src, &lint_cfg);
    match cmd {
        "check" => {
            print_lint(&sm, &lints);
            if !lints.is_empty() {
                eprintln!("{}", lint_summary(&lints));
            }
            // a rule the .sijolint raises to `error` fails the command, as `sijoc lint` does
            if lints.iter().any(|d| d.level == Level::Error) {
                return 1;
            }
            println!(
                "ok: {} archetype(s), {} component(s), {} system(s), {} function(s)",
                c.ecs.archetypes.len(),
                c.ecs.components.len(),
                c.ecs.systems.len(),
                c.prog.funcs.len()
            );
            0
        }
        "explain" => {
            print!("{}", crate::explain::report(&c, &sm));
            0
        }
        "build" | "run" => {
            if !lints.is_empty() {
                eprintln!("{}", lint_summary(&lints));
            }
            if lints.iter().any(|d| d.level == Level::Error) {
                print_lint(&sm, &lints.iter().filter(|d| d.level == Level::Error).cloned().collect::<Vec<_>>());
                return 1;
            }
            let stem = if inputs.len() == 1 && Path::new(&inputs[0]).is_dir() {
                std::fs::canonicalize(&inputs[0]).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| sm.name())
            } else {
                sm.name()
            };
            // host functions must come from somewhere: say so instead of a raw linker error
            let externs: Vec<String> = c.prog.funcs.iter().filter_map(|f| f.extern_name.clone()).collect();
            if !externs.is_empty() && cx.host.is_empty() && !(cx.lib && exe.is_none()) && !cx.ldflags.iter().any(|f| f.ends_with(".o") || f.ends_with(".a") || f.starts_with("-l")) {
                eprintln!(
                    "error: this program calls host functions (extern) that no C file defines: {}\n  pass --externs file.c (C definitions, keeps sijoc's main and its --server/--connect modes)\n  or --host file.c (your own main, embedding API), or --lib to only generate the C code",
                    externs.join(", ")
                );
                return 2;
            }
            // without -o: a private temporary directory, so concurrent builds never share files
            let tmp_dir = if out_c.is_none() {
                let d = std::env::temp_dir().join(format!("sijoc-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_nanos()).unwrap_or(0)));
                if let Err(e) = std::fs::create_dir_all(&d) {
                    eprintln!("error: cannot create {}: {}", d.display(), e);
                    return 2;
                }
                Some(d)
            } else {
                None
            };
            let out_c = out_c.unwrap_or_else(|| tmp_dir.as_ref().unwrap().join(format!("sijo_{}.c", stem)));
            let cname = out_c.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let prefix = cx.prefix.clone().unwrap_or_else(|| crate::codegen::DEFAULT_LIB_PREFIX.to_string());
            let csrc = crate::codegen::generate_full(&c, path, &sm, &cname, cx.lib, &prefix);
            if let Err(e) = std::fs::write(&out_c, csrc) {
                eprintln!("error: cannot write {}: {}", out_c.display(), e);
                return 2;
            }
            if cx.lib {
                let h = out_c.with_file_name(format!("{}.h", prefix));
                if let Err(e) = std::fs::write(&h, crate::codegen::embed_header(&c, &prefix)) {
                    eprintln!("error: cannot write {}: {}", h.display(), e);
                    return 2;
                }
                if cx.host.is_empty() && exe.is_none() {
                    println!("wrote {} and {} (embed: call {p}_create / {p}_step from your engine)", out_c.display(), h.display(), p = prefix);
                    return 0;
                }
                cx.cflags.push(format!("-I{}", h.parent().map(|d| d.display().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| ".".into())));
            }
            let exe = exe.unwrap_or_else(|| out_c.with_extension(""));
            match cc_compile_with(&cc, &out_c, &exe, openmp, &cx) {
                Ok(()) => {}
                Err(msg) => {
                    eprintln!("error: C compilation failed:\n{}", msg);
                    return 3;
                }
            }
            if cmd == "build" {
                println!("wrote {} and {}", out_c.display(), exe.display());
                return 0;
            }
            let status = Command::new(&exe).args(&prog_args).status();
            if let Some(d) = &tmp_dir {
                let _ = std::fs::remove_dir_all(d);
            }
            match status {
                Ok(s) => s.code().unwrap_or(1),
                Err(e) => {
                    eprintln!("error: cannot run {}: {}", exe.display(), e);
                    2
                }
            }
        }
        _ => usage(),
    }
}

/// extra C build options: embedding / host program / debug builds
#[derive(Default, Debug, Clone)]
pub struct CcOpts {
    /// generate a library (embedding API, no main)
    pub lib: bool,
    /// -g -O0 (with #line directives, gdb breakpoints and stack traces name .sijo lines)
    pub debug: bool,
    /// host C files compiled with the game (they provide main and extern functions)
    pub host: Vec<PathBuf>,
    pub cflags: Vec<String>,
    pub ldflags: Vec<String>,
    /// --lib-prefix: API function names (<prefix>_create ...) and header name (<prefix>.h)
    pub prefix: Option<String>,
}

fn cc_compile_with(cc: &str, c: &Path, exe: &Path, openmp: bool, cx: &CcOpts) -> Result<(), String> {
    let opt = if cx.debug { vec!["-O0".to_string(), "-g".into()] } else { vec!["-O2".to_string()] };
    // the generated C is alias-clean (no type punning except through memcpy), so gcc may use
    // type-based alias analysis: -fno-strict-aliasing cost up to 3x in field-heavy loops
    let mut base = vec!["-std=gnu11".to_string(), "-ffp-contract=off".into(), "-w".into()];
    base.extend(opt);
    base.extend(cx.cflags.iter().cloned());
    let try_omp = openmp;
    for attempt in 0..2 {
        let mut a = base.clone();
        if try_omp && attempt == 0 {
            a.push("-fopenmp".into());
        }
        a.push(c.display().to_string());
        for h in &cx.host {
            a.push(h.display().to_string());
        }
        a.push("-o".into());
        a.push(exe.display().to_string());
        a.push("-lm".into());
        if cc.contains("mingw") || cfg!(windows) {
            a.push("-lws2_32".into());
        }
        a.extend(cx.ldflags.iter().cloned());
        let out = Command::new(cc).args(&a).output().map_err(|e| format!("cannot run '{}': {}", cc, e))?;
        if out.status.success() {
            return Ok(());
        }
        if attempt == 1 || !try_omp {
            return Err(String::from_utf8_lossy(&out.stderr).to_string());
        }
    }
    base.clear();
    Ok(())
}
