//! Port-only test of the symbol ids that late-bound property names hold
//! (trunc1, from the infmemo1 skeptic fuzz case g106-36).
//!
//! Go gives a symbol its id on first use (`ast.GetSymbolId`): each read of
//! `valueSymbolLinks` (checker/links.go:33) and of a few node builder maps.
//! The checkers of a pool share one counter, and each `NewChecker` gives 4
//! checker symbols their ids before the first check
//! (checker/checker.go:1355 initializeChecker). The property name of a
//! unique symbol holds its id (`<prefix>@k4@<id>`, checker/checker.go:23402
//! getESSymbolLikeTypeForNode), and the node builder counts the length of
//! that name toward truncation (checker/nodebuilderimpl.go:2614
//! addPropertyToElementList). So the digits of the id move where
//! `... N more ...` starts (`SymbolArenaLinks`, `program::new_pool_checker`).
//!
//! Here `k4` has id 13 with the 2 checkers of the default pool (8 ids from
//! `NewChecker`, then `a0` to `a3` and `k4`) and id 9 with `--checkers 1`.
//! `skipLibCheck` leaves `globals.d.ts` unchecked, so only the checker of
//! `a.ts` gives ids after the pool is made, and the Go ids do not race. The
//! expected texts are the output of `tsgo-oracle-673a5f17d713 -p
//! tsconfig.json --pretty false` (with and without `--checkers 1`) on the
//! same files.
//!
//! followups38 adds the ids that `NewChecker` gives to binder symbols (merge
//! errors), which Go gives once in a pool.

use ts_goport::execute::tsc::ExitStatus;

use crate::support::child::run_command_in_child;
use crate::support::runner::TscInput;
use crate::support::test_sys::new_test_sys;

const PROJECT: &str = "/home/src/workspaces/project";

const GLOBALS: &str = "interface Array<T> { length: number; [n: number]: T }
interface Boolean {}
interface CallableFunction {}
interface Function {}
interface IArguments {}
interface NewableFunction {}
interface Number {}
interface Object {}
interface RegExp {}
interface String {}
";

/// The members `w0` to `w{count - 1}`, each with a space after it.
fn members(count: usize) -> String {
    (0..count).map(|i| format!("w{i}: number; ")).collect()
}

/// The `a.ts` of every test: `k4` follows 4 other symbols with ids, and a
/// name of `pad` letters `a` pads the type text.
fn a_ts(pad: usize) -> String {
    format!(
        "declare const a0: number;
declare const a1: number;
declare const a2: number;
declare const a3: number;
declare const k4: unique symbol;
declare const x: {{ [k4]: void; {}: number; {}q: string }};
const n: number = x;
",
        "a".repeat(pad),
        members(20)
    )
}

/// The diagnostics of `tsc -p tsconfig.json --pretty false` plus `extra`
/// on `files` (name and text) and `a_ts(pad)`, in that order.
fn check_files(files: &[(&str, String)], pad: usize, extra: &[&str]) -> String {
    let names: Vec<String> = files
        .iter()
        .map(|(name, _)| format!("\"{name}\""))
        .chain(["\"a.ts\"".to_string()])
        .collect();
    let tsconfig = format!(
        r#"{{"compilerOptions":{{"noLib":true,"skipLibCheck":true,"strict":true,"noEmit":true}},"files":[{}]}}"#,
        names.join(",")
    );
    let input = TscInput {
        files: files
            .iter()
            .map(|(name, text)| (format!("{PROJECT}/{name}"), text.clone().into()))
            .chain([
                (format!("{PROJECT}/a.ts"), a_ts(pad).into()),
                (format!("{PROJECT}/tsconfig.json"), tsconfig.into()),
            ])
            .collect(),
        ..Default::default()
    };
    let sys = new_test_sys(&input, false);
    let mut args: Vec<String> = ["-p", "tsconfig.json", "--pretty", "false"]
        .map(String::from)
        .to_vec();
    args.extend(extra.iter().map(|arg| arg.to_string()));
    let result = run_command_in_child(&sys, &args).unwrap_or_else(|err| panic!("tsgo: {err}"));
    assert!(result.unported.is_none(), "unported {:?}", result.unported);
    assert_eq!(
        result.status,
        ExitStatus::DiagnosticsPresentOutputsGenerated
    );
    // The test system adds the list of files after the diagnostics.
    let output = sys.output_text();
    output
        .split("!!! List files start")
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The diagnostics of `tsc -p tsconfig.json --pretty false` plus `extra`.
fn check(extra: &[&str]) -> String {
    check_files(&[("globals.d.ts", GLOBALS.into())], 9, extra)
}

/// Go's error with the members up to `w{shown - 1}` and `more` hidden.
fn expected(shown: usize, more: usize) -> String {
    expected_with_pad(9, shown, more)
}

/// `expected` for `a_ts(pad)`.
fn expected_with_pad(pad: usize, shown: usize, more: usize) -> String {
    format!(
        "a.ts(7,7): error TS2322: Type '{{ [k4]: void; {}: number; {}... {more} more ...; q: string; }}' is not assignable to type 'number'.\n",
        "a".repeat(pad),
        members(shown)
    )
}

#[test]
fn unique_symbol_ids_count_as_go_in_the_default_pool() {
    // Id 13: the name is one byte longer than with id 9, so w14 goes.
    assert_eq!(check(&[]), expected(14, 6));
}

#[test]
fn unique_symbol_ids_count_as_go_with_one_checker() {
    assert_eq!(check(&["--checkers", "1"]), expected(15, 5));
}

/// `declare let d0: number;` to `d{count - 1}`, one per line.
fn lets(count: usize) -> String {
    (0..count)
        .map(|i| format!("declare let d{i}: number;\n"))
        .collect()
}

#[test]
fn pool_checkers_skip_only_the_ids_of_their_own_symbols() {
    // Each `NewChecker` merges the globals, and its merge errors (TS2451,
    // hidden by skipLibCheck) give `d0` to `d25` their ids. They are binder
    // symbols, so Go gives each id once in the pool of 4 checkers: k4 gets
    // id 47 (4 * 4 + 26 + 5), not 125 ((4 + 26) * 4 + 5), and with a pad
    // of 8 w14 stays. Go's merge errors race in the pool, so a few ids can
    // burn (k4 49), but k4 keeps 2 digits.
    let files = [
        ("globals.d.ts", format!("{GLOBALS}{}", lets(26))),
        ("dup1.d.ts", lets(26)),
        ("dup2.d.ts", "declare const z: number;\n".into()),
    ];
    assert_eq!(check_files(&files, 8, &[]), expected_with_pad(8, 15, 5));
}
