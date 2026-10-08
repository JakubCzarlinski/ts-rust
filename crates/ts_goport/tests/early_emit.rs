//! The early emit of `tsc -p` and `tsc -b` with an incremental program
//! (`execute::incremental::Program::start_emit`). Each checker gets its emit
//! jobs right behind its check job, so it emits when its own check ends and
//! the emit pool runs during the check. The outputs, the build info, stdout
//! and the exit code must be the same as with `GOPORT_EARLY_EMIT=0`, which
//! keeps Go's barrier (the emit starts after the whole check).
//!
//! The fixture is `fixtures/emit_pool`: 4 program files and the es2020 lib
//! files, so 4 checkers get files. With declarations the JS parts of
//! `shapes.ts`, `legacy.js` and `index.ts` go to the emit pool and the d.ts
//! parts stay on the checker threads. `GOPORT_EMIT_THREADS=2` turns the pool
//! on at any core count. Each `tsgo` run writes to the same new directory
//! under the system temp dir, so the source map and build info paths are
//! the same. A passing test deletes it.
//!
//! The rule test checks `emit_can_start_with_check` on the same fixture:
//! each case that a check could see the outputs of keeps the barrier. Do not
//! set `GOPORT_EARLY_EMIT=0` for this test. Its F2 cases load
//! `tsconfig.rules.json` (the same config with `"exclude": []`): without an
//! `exclude`, the config excludes `outDir` and `declarationDir` from its
//! files, so no program file would be inside them.
//!
//! The emit-only tests make their own `tsc -b` solution in a scratch dir: a
//! task that checks nothing (every file's semantic diagnostics cached,
//! `noCheck`, or a syntax error) must emit on its checker threads too, so it
//! finishes when its own emit ends, as a Go builder does. The global
//! diagnostics test makes its own project there too. So do the k2gaps1
//! tests: a task without the incremental state (a project that is not
//! `incremental` or `composite`) writes when its emit ends, `tsc -p
//! --listFilesOnly` emits nothing, and `tsc -b` of two small projects runs
//! without a panic (also in the dev profile). The barrier test loads the
//! fixture and checks that `send_checker_barrier` waits for the emit pool
//! and the d.ts twins.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use ts_goport::core::enter_program;
use ts_goport::emitter::program_emit::emit_can_start_with_check;
use ts_goport::flags::{ModuleKind, ModuleResolutionKind};
use ts_goport::options::{CompilerOptions, Tristate};
use ts_goport::program::{release_program, try_load_version};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/emit_pool");

const CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/emit_pool/tsconfig.json"
);

/// `CONFIG` with `"exclude": []`, so program files can be in the output
/// directories.
const RULES_CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/emit_pool/tsconfig.rules.json"
);

/// The tests that load a program in this process (the rule test and the
/// barrier test) take this lock: two program loads at once in one process
/// break the node store and file id checks of `ast/store.rs`.
static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What one `tsgo` run wrote and printed.
#[derive(Debug, PartialEq)]
struct Run {
    /// The bytes of each file under the out dir, by relative path.
    files: BTreeMap<String, Vec<u8>>,
    stdout: String,
    status: Option<i32>,
}

#[test]
fn early_emit_writes_what_the_barrier_writes() {
    let root = scratch_dir();
    let out = root.join("out");
    let cases: [(&str, &[&str]); 3] = [
        ("js and d.ts", &[]),
        (
            "js only",
            &["--declaration", "false", "--declarationMap", "false"],
        ),
        ("d.ts only", &["--emitDeclarationOnly"]),
    ];
    for (case, extra) in cases {
        let barrier = tsgo(&out, extra, false);
        let early = tsgo(&out, extra, true);
        assert_eq!(
            barrier.status,
            Some(0),
            "{case}: the fixture must compile without diagnostics, so the check is sent: {}",
            barrier.stdout
        );
        assert!(
            barrier.files.contains_key("tsconfig.tsbuildinfo")
                && barrier.files.keys().any(|name| std::path::Path::new(name)
                    .extension()
                    .is_some_and(|e| e == "js")
                    || name.ends_with(".d.ts")),
            "{case}: the run must write outputs and build info: {:?}",
            barrier.files.keys()
        );
        assert_eq!(early, barrier, "{case}: the early emit against the barrier");
    }
    fs::remove_dir_all(&root).unwrap_or_else(|error| panic!("remove {}: {error}", root.display()));
}

/// `tsc -b` of the fixture as a composite project, and (k2gaps1, G4) as a
/// project that is not `incremental` or `composite`: it has no incremental
/// state, so its early emit is the emit of the whole program
/// (`start_emit_files`), with and without declarations (without them every
/// JS part runs on the emit pool).
#[test]
fn build_early_emit_writes_what_the_barrier_writes() {
    let root = scratch_dir();
    let out = root.join("out");
    let cases = [
        ("composite", r#""composite": true,"#),
        ("not incremental", ""),
        (
            "not incremental, js only",
            r#""declaration": false, "declarationMap": false,"#,
        ),
    ];
    for (case, options) in cases {
        // The fixture with `options`, its outputs and build info in the
        // scratch dir. The base config's `include` and `rootDir` stay
        // relative to the fixture.
        let config = root.join("tsconfig.json");
        let text = format!(
            r#"{{
  "extends": "{FIXTURE}/tsconfig.json",
  "compilerOptions": {{
    {options}
    "outDir": "{out}",
    "tsBuildInfoFile": "{out}/tsconfig.tsbuildinfo"
  }}
}}
"#,
            out = out.display()
        );
        fs::write(&config, text)
            .unwrap_or_else(|error| panic!("write {}: {error}", config.display()));
        let barrier = tsgo_build(&config, &out, false);
        let early = tsgo_build(&config, &out, true);
        assert_eq!(
            barrier.status,
            Some(0),
            "{case}: the fixture must build without diagnostics, so the check is sent: {}",
            barrier.stdout
        );
        assert!(
            barrier.files.contains_key("tsconfig.tsbuildinfo")
                && barrier.files.keys().any(|name| std::path::Path::new(name)
                    .extension()
                    .is_some_and(|e| e == "js")
                    || name.ends_with(".d.ts")),
            "{case}: the build must write outputs and build info: {:?}",
            barrier.files.keys()
        );
        assert_eq!(early, barrier, "{case}: the early emit against the barrier");
    }
    fs::remove_dir_all(&root).unwrap_or_else(|error| panic!("remove {}: {error}", root.display()));
}

/// K2 (tscbemit1). `tsc -b --builders 2` on the solution p1 p2 p3, with no
/// references. Only the emits of p1 and p2 are pending (`--noEmit` builds
/// after an edit checked them): p1 is one root with a large emit, p2 a small
/// writer whose new output adds `v2`, and p3 imports `v2` from p2's output.
/// A Go builder writes its task's outputs when the task's emit ends, so p2
/// ends first, its builder takes p3, and p3 loads after p2 wrote: exit 0,
/// no output (Go N gives that). Before tscbemit1 a task with nothing to
/// check emitted on the loading thread when it finished, so p1 and p2
/// finished in build order and p3 loaded before p2 wrote (TS2305).
#[test]
fn build_emit_only_task_finishes_when_its_emit_ends() {
    assert_eq!(
        build_emit_only_solution("", &[], 0),
        (Some(0), String::new()),
        "p2 finishes before p1, so p3 loads after p2 wrote v2"
    );
}

/// tscbemit2: as `build_emit_only_task_finishes_when_its_emit_ends`, with
/// `noCheck` in p1. Go's p1 does no semantic check either
/// (`GetSemanticDiagnostics` returns nil), so it only emits: exit 0, no
/// output (Go N gives that). Before tscbemit2 the port started no early
/// emit for it, so it finished first, in build order (TS2305).
#[test]
fn build_no_check_task_finishes_when_its_emit_ends() {
    assert_eq!(
        build_emit_only_solution(r#", "noCheck": true"#, &[], 0),
        (Some(0), String::new()),
        "p2 finishes before p1, so p3 loads after p2 wrote v2"
    );
}

/// tscbemit2: as `build_emit_only_task_finishes_when_its_emit_ends`, with a
/// syntax error in p1. Go's `GetDiagnosticsOfAnyProgram` stops at the
/// syntactic diagnostics, so p1 only emits: only p1's TS1109 (Go N gives
/// that). Before tscbemit2 the port started no early emit for it, so it
/// finished first, in build order, and p3 also gave TS2305.
#[test]
fn build_task_with_syntax_errors_finishes_when_its_emit_ends() {
    assert_eq!(
        build_emit_only_solution("", &[("bad.ts", "export const bad = ;\n")], 2),
        (
            Some(2),
            "p1/src/bad.ts(1,20): error TS1109: Expression expected.\n".to_owned()
        ),
        "p2 finishes before p1, so p3 loads after p2 wrote v2"
    );
}

/// tscbemit2: Go reads the global diagnostics before the emit, also with
/// `noCheck` or program diagnostics (here TS6053, a missing file), where
/// `start_check` does not read them, so the early emit reads them first.
/// With `lib` es5 the d.ts emit of a generator asks for the missing global
/// type `IterableIterator`, which adds TS2318 to the checker's global
/// diagnostics. Go never reports it (it read them before), so `tsc -p` and
/// `tsc -b` give only the program diagnostics (Go N gives that). A read after
/// the early emit adds TS2318.
#[test]
fn early_emit_reads_the_global_diagnostics_before_the_emit() {
    let missing = concat!(
        "error TS6053: File '{root}/src/missing.ts' not found.\n",
        "  The file is in the program because:\n",
        "    Part of 'files' list in tsconfig.json\n"
    );
    for (options, files, expected) in [
        (r#", "noCheck": true"#, r#""include": ["src"]"#, None),
        (
            "",
            r#""files": ["src/a.ts", "src/missing.ts"]"#,
            Some(missing),
        ),
    ] {
        let root = scratch_dir();
        let config = format!(
            r#"{{"compilerOptions": {{"composite": true, "strict": true, "target": "es2022",
  "module": "esnext", "moduleResolution": "bundler", "outDir": "dist", "rootDir": "src",
  "skipLibCheck": true, "lib": ["es5"]{options}}}, {files}}}"#
        );
        fs::write(root.join("tsconfig.json"), config).expect("write the config");
        fs::create_dir(root.join("src")).expect("create src");
        fs::write(root.join("src/a.ts"), "export function* g() { yield 1; }\n")
            .expect("write a.ts");
        let expected = expected.map_or_else(String::new, |text| {
            text.replace("{root}", &root.display().to_string())
        });
        let status = if expected.is_empty() { 0 } else { 2 };
        for args in [
            &["-p", "tsconfig.json"][..],
            &["-b", "tsconfig.json", "--force"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_tsgo"))
                .current_dir(&root)
                .args(args)
                .args(["--pretty", "false"])
                .env("GOPORT_EARLY_EMIT", "1")
                .output()
                .expect("run tsgo");
            assert_eq!(
                (
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout).into_owned()
                ),
                (Some(status), expected.clone()),
                "tsgo {args:?} with{options} and {files}: no TS2318 from the emit"
            );
        }
        fs::remove_dir_all(&root)
            .unwrap_or_else(|error| panic!("remove {}: {error}", root.display()));
    }
}

/// k2gaps1 (M6): `tsc -p --listFilesOnly` on an incremental project lists
/// the program files, exits 0 and writes nothing (Go N gives that). The
/// early emit rules refuse `--listFilesOnly` (`early_emit_options_allow`;
/// `rules_keep_the_barrier_when_a_check_could_see_the_outputs` checks the
/// rule). An emit that started anyway would write only when it ended
/// before the process exits.
#[test]
fn tsc_p_list_files_only_writes_no_output() {
    let root = scratch_dir();
    fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions": {"incremental": true, "strict": true, "target": "es2022",
  "module": "esnext", "moduleResolution": "bundler", "outDir": "dist", "rootDir": "src",
  "declaration": true, "skipLibCheck": true, "lib": ["es5"]}, "include": ["src"]}"#,
    )
    .expect("write the config");
    fs::create_dir(root.join("src")).expect("create src");
    fs::write(root.join("src/a.ts"), "export const a = 1;\n").expect("write a.ts");
    let output = Command::new(env!("CARGO_BIN_EXE_tsgo"))
        .current_dir(&root)
        .args([
            "-p",
            "tsconfig.json",
            "--listFilesOnly",
            "--pretty",
            "false",
        ])
        .env("GOPORT_EARLY_EMIT", "1")
        .output()
        .expect("run tsgo");
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned()
        ),
        (
            Some(0),
            format!(
                "bundled:///libs/lib.es5.d.ts\nbundled:///libs/lib.decorators.d.ts\n\
                 bundled:///libs/lib.decorators.legacy.d.ts\n{}/src/a.ts\n",
                root.display()
            )
        ),
        "tsc -p --listFilesOnly lists the files"
    );
    let mut files = BTreeMap::new();
    read_files(&root, &root, &mut files);
    assert_eq!(
        files.keys().collect::<Vec<_>>(),
        ["src/a.ts", "tsconfig.json"],
        "tsc -p --listFilesOnly writes nothing"
    );
    fs::remove_dir_all(&root).unwrap_or_else(|error| panic!("remove {}: {error}", root.display()));
}

/// k2gaps1 (G4): as `build_no_check_task_finishes_when_its_emit_ends`, with
/// projects that are not `incremental` or `composite` (`tsc -b p1 p2 p3`,
/// `--builders 2`): p1 big and `noCheck`, p2 a small `noCheck` writer
/// whose new output adds `v2`, p3 its reader. They have no incremental
/// state, so Go's task emits the whole program, and writes when that emit
/// ends: p2 ends first, its builder takes p3, and p3 loads after p2 wrote.
/// Exit 0, no output (Go N gives that). Before, such a task had no checker
/// work and emitted on the loading thread when it finished, in build
/// order, so p1 finished first and p3 loaded before p2 wrote (TS2305).
#[test]
fn build_non_incremental_task_finishes_when_its_emit_ends() {
    let solution = Solution::new();
    solution.write("p1/tsconfig.json", &non_incremental_config(NO_CHECK));
    solution.write("p2/tsconfig.json", &non_incremental_config(NO_CHECK));
    solution.write("p3/tsconfig.json", &non_incremental_config(""));
    solution.write("p1/src/index.ts", &big_module("v1", 1500));
    solution.write("p2/src/index.ts", "export const v1 = 1;\n");
    solution.write(
        "p3/src/a.ts",
        "import { v1, v2 } from \"../../p2/dist/index\";\nexport const a = v1 + v2;\n",
    );
    solution.build(&["p1", "p2", "p3"]);
    solution.write("p1/src/index.ts", &big_module("v2", 1500));
    solution.write(
        "p2/src/index.ts",
        "export const v1 = 1;\nexport const v2 = 2;\n",
    );
    assert_eq!(
        solution.build(&["p1", "p2", "p3", "--builders", "2"]),
        (Some(0), String::new()),
        "p2 finishes before p1, so p3 loads after p2 wrote v2"
    );
    solution.remove();
}

/// k2gaps1 (G4): the other way round. p1 is the big `noCheck` writer, whose
/// new output adds `v2`; p2 only emits a small file; p3 reads `v2` from
/// p1's output (`tsc -b p1 p2 p3 --builders 2`, not `incremental` or
/// `composite`). Go's p1 writes when its emit ends, after p2 ended and its
/// builder took p3, so p3 loads before p1 writes: TS2305 (Go N gives
/// that). Before, the port emitted p1 and wrote its outputs as soon as it
/// finished, first, so p3 saw `v2` (exit 0).
#[test]
fn build_non_incremental_writer_writes_when_its_emit_ends() {
    let solution = Solution::new();
    solution.write("p1/tsconfig.json", &non_incremental_config(NO_CHECK));
    solution.write("p2/tsconfig.json", &non_incremental_config(NO_CHECK));
    solution.write("p3/tsconfig.json", &non_incremental_config(""));
    solution.write(
        "p1/src/index.ts",
        &(big_module("v1", 1500) + "export const v1 = 1;\n"),
    );
    solution.write("p2/src/index.ts", "export const s = 1;\n");
    solution.write(
        "p3/src/a.ts",
        "import { v1, v2 } from \"../../p1/dist/index\";\nexport const a = v1 + v2;\n",
    );
    solution.build(&["p1", "p2", "p3"]);
    solution.write(
        "p1/src/index.ts",
        &(big_module("v2", 1500) + "export const v1 = 1;\nexport const v2 = 2;\n"),
    );
    solution.write(
        "p2/src/index.ts",
        "export const s = 1;\nexport const t = 2;\n",
    );
    assert_eq!(
        solution.build(&["p1", "p2", "p3", "--builders", "2"]),
        (
            Some(2),
            "p3/src/a.ts(1,14): error TS2305: Module '\"../../p1/dist/index\"' has no exported member 'v2'.\n"
                .to_owned()
        ),
        "p2 finishes first, so p3 loads before p1 wrote v2"
    );
    solution.remove();
}

/// k2gaps1 (C1): `program::send_checker_barrier` makes one value per
/// checker thread and one for the emit pool, and each drops only when the
/// jobs sent before have ended: the checker thread's own jobs, the jobs of
/// its d.ts twin (the d.ts prints, which wait for their JS parts on the
/// pool) and the emit pool jobs. `tsc -b` finishes a task when all its
/// values have dropped (`BuildTask::notify_when_compiled`), so a task with
/// a long emit on the pool or a twin finishes when that emit ends, as its
/// Go builder does. Here one twin job and one pool job wait on a gate, so
/// two values must stay until their gates open. `send_emit_pool_jobs` and
/// `send_dts_twin_job` make the pool and the twin at any core count.
#[test]
fn checker_barrier_waits_for_the_emit_pool_and_the_twins() {
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;
    use ts_goport::program::{
        run_on_checker_threads_for_files, send_checker_barrier, send_dts_twin_job,
        send_emit_pool_jobs, source_files,
    };

    /// A job waits in `pass` until `open` runs.
    #[derive(Clone, Default)]
    struct Gate(Arc<(Mutex<bool>, Condvar)>);
    impl Gate {
        fn pass(&self) {
            let (open, changed) = &*self.0;
            let mut open = open.lock().expect("gate lock");
            while !*open {
                open = changed.wait(open).expect("gate lock");
            }
        }
        fn open(&self) {
            *self.0.0.lock().expect("gate lock") = true;
            self.0.1.notify_all();
        }
    }
    /// Sends one `()` when the barrier drops it.
    struct Dropped(Sender<()>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    /// The values that drop within `wait`.
    fn dropped(receiver: &std::sync::mpsc::Receiver<()>, wait: Duration) -> usize {
        std::iter::from_fn(|| receiver.recv_timeout(wait).ok()).count()
    }

    let _in_process = IN_PROCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let program = try_load_version(CONFIG, |_| {})
        .unwrap_or_else(|error| panic!("cannot load {CONFIG}: {error}"));
    {
        let _scope = enter_program(Some(program));
        let twin_gate = Gate::default();
        let pool_gate = Gate::default();
        let file = *source_files().last().expect("a program file");
        let gate = twin_gate.clone();
        let twin_jobs = run_on_checker_threads_for_files(&[file], move |_| {
            let gate = gate.clone();
            send_dts_twin_job(move || gate.pass())
        });
        let gate = pool_gate.clone();
        let pool_jobs = send_emit_pool_jobs(vec![move || gate.pass()]);
        let (sender, receiver) = channel();
        let values = send_checker_barrier(|| Dropped(sender.clone()));
        drop(sender);
        assert!(values >= 2, "one value per checker and one for the pool");

        let short = Duration::from_millis(300);
        assert_eq!(
            dropped(&receiver, short),
            values - 2,
            "the values of the waiting twin and the waiting pool job stay"
        );
        twin_gate.open();
        let long = Duration::from_secs(30);
        receiver
            .recv_timeout(long)
            .expect("the twin's value drops when its job ends");
        assert_eq!(
            dropped(&receiver, short),
            0,
            "the pool's value stays while its job waits"
        );
        pool_gate.open();
        receiver
            .recv_timeout(long)
            .expect("the pool's value drops when its job ends");
        drop((twin_jobs, pool_jobs));
    }
    release_program(program);
}

/// k2gaps1: `tsc -b` of two small composite projects (a cold build, then
/// an edit) exits 0 and prints nothing. Run it in the dev profile too
/// (`cargo test -p ts_goport --test early_emit`): there the
/// `debug_assert!`s of the early emit run, and the release build of the
/// protected tests leaves them out. In round 1 of k2gaps1 one of them
/// fired in every dev-profile `tsc -b`.
#[test]
fn build_two_composite_projects_without_a_panic() {
    let solution = Solution::new();
    solution.write(
        "tsconfig.json",
        r#"{"files": [], "references": [{"path": "./p1"}, {"path": "./p2"}]}"#,
    );
    solution.write("p1/tsconfig.json", &project_config(""));
    solution.write(
        "p2/tsconfig.json",
        r#"{"compilerOptions": {"composite": true, "strict": true, "target": "es2022",
  "module": "esnext", "moduleResolution": "bundler", "outDir": "dist", "rootDir": "src",
  "skipLibCheck": true}, "include": ["src"], "references": [{"path": "../p1"}]}"#,
    );
    solution.write("p1/src/index.ts", "export const v1 = 1;\n");
    solution.write(
        "p2/src/a.ts",
        "import { v1 } from \"../../p1/src/index\";\nexport const a = v1;\n",
    );
    assert_eq!(
        solution.build(&["tsconfig.json"]),
        (Some(0), String::new()),
        "the cold build"
    );
    solution.write("p1/src/index.ts", "export const v1 = 2;\n");
    assert_eq!(
        solution.build(&["tsconfig.json"]),
        (Some(0), String::new()),
        "the build after an edit"
    );
    solution.remove();
}

/// The `noCheck` option for `project_config` and `non_incremental_config`.
const NO_CHECK: &str = r#", "noCheck": true"#;

/// The solution config of p1, p2 and p3.
const SOLUTION: &str =
    r#"{"files": [], "references": [{"path": "./p1"}, {"path": "./p2"}, {"path": "./p3"}]}"#;

/// A `tsc -b` solution in a scratch dir.
struct Solution {
    root: PathBuf,
}

impl Solution {
    fn new() -> Self {
        Solution {
            root: scratch_dir(),
        }
    }

    /// Writes `text` to `path` under the root, and makes its directories.
    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().expect("a file in a project"))
            .unwrap_or_else(|error| panic!("create the dir of {}: {error}", path.display()));
        fs::write(&path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    }

    /// Runs `tsgo -b` with `args` and `--pretty false`, and returns its exit
    /// code and stdout.
    fn build(&self, args: &[&str]) -> (Option<i32>, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_tsgo"))
            .current_dir(&self.root)
            .arg("-b")
            .args(args)
            .args(["--pretty", "false"])
            .env("GOPORT_EARLY_EMIT", "1")
            .output()
            .expect("run tsgo -b");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    /// Removes the scratch dir. A failed test keeps it.
    fn remove(self) {
        fs::remove_dir_all(&self.root)
            .unwrap_or_else(|error| panic!("remove {}: {error}", self.root.display()));
    }
}

/// The config of a solution project, with `extra` added to its compiler
/// options.
fn project_config(extra: &str) -> String {
    format!(
        r#"{{"compilerOptions": {{"composite": true, "strict": true, "target": "es2022",
  "module": "esnext", "moduleResolution": "bundler", "outDir": "dist", "rootDir": "src",
  "skipLibCheck": true{extra}}}, "include": ["src"]}}"#
    )
}

/// The config of a project that is not `incremental` or `composite` (so
/// `tsc -b` gives it no incremental state), with declarations and `extra`
/// added to its compiler options.
fn non_incremental_config(extra: &str) -> String {
    format!(
        r#"{{"compilerOptions": {{"declaration": true, "strict": true, "target": "es2022",
  "module": "esnext", "moduleResolution": "bundler", "outDir": "dist", "rootDir": "src",
  "skipLibCheck": true{extra}}}, "include": ["src"]}}"#
    )
}

/// The code of `count` modules in one file, with `tag` in each comment: its
/// load and emit take far longer than those of a small project.
fn big_module(tag: &str, count: usize) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for i in 0..count {
        write!(
            text,
            "export interface I{i} {{ a: number; b: string; c{i}: boolean }}\n\
             export function f{i}(x: I{i}): I{i} {{ return {{ ...x }}; }}\n\
             export class C{i} {{ constructor(public v: I{i}) {{}} get(): I{i} {{ return f{i}(this.v); }} }}\n\
             export const k{i}: number = {i}; // {tag}\n"
        )
        .expect("write to a String");
    }
    text
}

/// Makes the solution of `build_emit_only_task_finishes_when_its_emit_ends`
/// in a scratch dir, with `p1_options` added to p1's compiler options and
/// `p1_files` added to p1's `src`, runs its builds and returns the exit code
/// and stdout of the last one (`--builders 2`). `p1_no_emit` is Go N's exit
/// code of `tsc -b p1 --noEmit` on it.
fn build_emit_only_solution(
    p1_options: &str,
    p1_files: &[(&str, &str)],
    p1_no_emit: i32,
) -> (Option<i32>, String) {
    let solution = Solution::new();
    solution.write("tsconfig.json", SOLUTION);
    solution.write("p1/tsconfig.json", &project_config(p1_options));
    for project in ["p2", "p3"] {
        solution.write(&format!("{project}/tsconfig.json"), &project_config(""));
    }
    // Its emit takes far longer than the load and emit of p2.
    solution.write("p1/src/index.ts", &big_module("v1", 1500));
    for (name, text) in p1_files {
        solution.write(&format!("p1/src/{name}"), text);
    }
    solution.write("p2/src/s0.ts", "export const s0 = 0;\n");
    solution.write("p2/src/s1.ts", "export const s1 = 1;\n");
    solution.write("p2/src/index.ts", "export const v1 = 1;\n");
    solution.write(
        "p3/src/a.ts",
        "import { v1, v2 } from \"../../p2/dist/index\";\nexport const a = v1 + v2;\n",
    );
    // The cold build: p3 cannot see `v2` yet.
    solution.build(&["tsconfig.json"]);
    solution.write("p1/src/index.ts", &big_module("v2", 1500));
    solution.write(
        "p2/src/index.ts",
        "export const v1 = 1;\nexport const v2 = 2;\n",
    );
    // p1's errors are part of the case; p2 must check clean.
    let (status, stdout) = solution.build(&["p1", "--noEmit"]);
    assert_eq!(status, Some(p1_no_emit), "tsc -b p1 --noEmit: {stdout}");
    let (status, stdout) = solution.build(&["p2", "--noEmit"]);
    assert_eq!(status, Some(0), "tsc -b p2 --noEmit: {stdout}");
    let result = solution.build(&["tsconfig.json", "--builders", "2"]);
    solution.remove();
    result
}

#[test]
fn rules_keep_the_barrier_when_a_check_could_see_the_outputs() {
    let _in_process = IN_PROCESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let out_dir = std::env::temp_dir()
        .join("goport-early-emit-rules")
        .to_string_lossy()
        .into_owned();
    // As in `early_emit_writes_what_the_barrier_writes`.
    let out = out_dir.clone();
    assert!(
        can_start(move |options| options.out_dir = out),
        "the fixture with a temp outDir must start its emit with the check"
    );

    // F1: `index.ts` imports "./shapes" without an extension.
    assert!(!can_start(|options| {
        options.module = ModuleKind::NODE_NEXT;
        options.module_resolution = ModuleResolutionKind::NODE_NEXT;
    }));
    // F2: the program files are inside the outDir or declarationDir.
    assert!(!can_start_with(RULES_CONFIG, |options| {
        options.out_dir = format!("{FIXTURE}/src");
    }));
    assert!(!can_start_with(RULES_CONFIG, |options| {
        options.declaration_dir = FIXTURE.to_string();
    }));
    // F3: an output directory under `node_modules`.
    let under_node_modules = format!("{out_dir}/node_modules/out");
    assert!(!can_start(move |options| {
        options.out_dir = under_node_modules;
    }));
    // F4 and the option rules.
    assert!(!can_start(|options| {
        options.preserve_symlinks = Tristate::True;
    }));
    assert!(!can_start(|options| {
        options.no_emit_on_error = Tristate::True;
    }));
    assert!(!can_start(|options| {
        options.single_threaded = Tristate::True;
    }));
    // k2gaps1 (M6): Go emits nothing with `--listFilesOnly`.
    assert!(!can_start(|options| {
        options.list_files_only = Tristate::True;
    }));
}

/// Loads the fixture with `edit` applied to its options and returns
/// `emit_can_start_with_check` for it.
fn can_start(edit: impl FnOnce(&mut CompilerOptions)) -> bool {
    can_start_with(CONFIG, edit)
}

/// `can_start` with the fixture config `config`.
fn can_start_with(config: &str, edit: impl FnOnce(&mut CompilerOptions)) -> bool {
    let program = try_load_version(config, edit)
        .unwrap_or_else(|error| panic!("cannot load {config}: {error}"));
    let can_start = {
        let _scope = enter_program(Some(program));
        emit_can_start_with_check()
    };
    release_program(program);
    can_start
}

/// Runs `tsgo -p` on the fixture as an incremental program, with `out` as
/// the out dir and the build info in it, and `extra` arguments. `early`
/// false sets `GOPORT_EARLY_EMIT=0`. It removes `out` first and returns what
/// the run wrote there and printed.
fn tsgo(out: &Path, extra: &[&str], early: bool) -> Run {
    if out.exists() {
        fs::remove_dir_all(out).unwrap_or_else(|error| panic!("remove {}: {error}", out.display()));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_tsgo"))
        .args(["-p", CONFIG, "--incremental", "--outDir"])
        .arg(out)
        .arg("--tsBuildInfoFile")
        .arg(out.join("tsconfig.tsbuildinfo"))
        .args(["--listEmittedFiles", "--pretty", "false"])
        .args(extra)
        .env("GOPORT_EMIT_THREADS", "2")
        .env("GOPORT_EARLY_EMIT", if early { "1" } else { "0" })
        .output()
        .expect("run tsgo");
    let mut files = BTreeMap::new();
    if out.exists() {
        read_files(out, out, &mut files);
    }
    Run {
        files,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        status: output.status.code(),
    }
}

/// Runs `tsgo -b` on `config`, whose outputs and build info are in `out`.
/// `early` false sets `GOPORT_EARLY_EMIT=0`. It removes `out` first, so the
/// project is out of date, and returns what the run wrote there and printed.
fn tsgo_build(config: &Path, out: &Path, early: bool) -> Run {
    if out.exists() {
        fs::remove_dir_all(out).unwrap_or_else(|error| panic!("remove {}: {error}", out.display()));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_tsgo"))
        .arg("-b")
        .arg(config)
        .args(["--listEmittedFiles", "--pretty", "false"])
        .env("GOPORT_EMIT_THREADS", "2")
        .env("GOPORT_EARLY_EMIT", if early { "1" } else { "0" })
        .output()
        .expect("run tsgo -b");
    let mut files = BTreeMap::new();
    if out.exists() {
        read_files(out, out, &mut files);
    }
    Run {
        files,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        status: output.status.code(),
    }
}

/// Reads every file under `dir` into `files`, by path relative to `root`.
fn read_files(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let path = entry.expect("out dir entry").path();
        if path.is_dir() {
            read_files(root, &path, files);
        } else {
            let name = path
                .strip_prefix(root)
                .expect("a path under the out dir")
                .to_string_lossy()
                .into_owned();
            let bytes =
                fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            files.insert(name, bytes);
        }
    }
}

/// A new directory under the system temp dir, by its real path (the
/// program sees real paths).
fn scratch_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("goport-early-emit-{}-{nanos}", std::process::id()));
    fs::create_dir(&dir).unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    fs::canonicalize(&dir).expect("canonical scratch dir")
}
