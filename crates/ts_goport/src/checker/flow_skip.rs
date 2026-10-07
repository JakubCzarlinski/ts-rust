//! flowskip1: skip a flow walk that can only return the declared type and
//! makes nothing on its way (Bun `is_never_narrowed`,
//! `sema/check/flow.rs:4766-4886`, made exact against Go's walk).
//!
//! The Go model is in `studies/flowskip1/go-model.md` with the fixes in
//! `studies/flowskip1/go-model-fix.md` (under
//! `target/continuation-r97-goport`). In short: when every flow node that
//! Go's walk visits is inert for the reference R, `getTypeAtFlowNode`
//! (flow.go:117) visits a fixed path pi(u) from the flow node u of R:
//! - assignments, calls and array mutations continue at their antecedent;
//! - conditions and switch clauses nest on their antecedent;
//! - a branch label nests on its first walked antecedent (the first empty
//!   switch clause is the bypass, flow.go:1260), and the declared type
//!   returns at once there because declared == initial (flow.go:1269);
//! - a Start node moves to the containing function when Go's rule holds
//!   (flow.go:189), else gives the initial type, which is the declared type.
//!
//! On such a path Go makes no type, symbol, signature or diagnostic, writes
//! no lasting state and returns the declared type. A node is inert for R
//! when no role of its expression can match R (mention families), every
//! identifier that Go resolves there for R's class is resolved already, a
//! call has a cached "none" effects signature (flow.go:2047), and the node
//! has no key name resolution (flow.go:1753) and no union-making `&&`/`||`
//! (flow.go:539-551).
//!
//! Index (per checker, per static file): the forest of "next" pointers of
//! pi, an Euler tour of it, and per mention key the sorted entry and exit
//! numbers of the nodes that mention it, so a count of mentions on a path
//! is two binary searches. Per class of reference (kind x union declared
//! type x constant reference), candidates that are settled and have nothing
//! to check per walk are joined to their parent (union-find), so a walk
//! over settled nodes costs about O(1). A file is indexed only once its
//! walks have taken 16 steps per flow node (sampled), so files with short
//! walks pay almost nothing.
//!
//! flowskip2 (`studies/flowskip1/go-model-2.md` with the fixes in
//! `go-model-2-fix.md`) adds three ends to pi(u):
//! - M, the declaration of the reference's root symbol (flow.go:224-266):
//!   Go matches there, runs `isReachableFlowNode(M)` (flow.go:2513) and
//!   returns the declared type. The skip dry-runs that call on the current
//!   caches and commits what Go writes (`lastFlowNode`, `flowNodeReachable`).
//! - a loop label with 2+ antecedents (flow.go:1325): Go reads
//!   `flowLoopCache`, then walks the entry only and writes the cache. The
//!   skip reads the cache and plans the same writes.
//! - for an identifier whose initial type differs from its declared type
//!   (Variant B), Go walks every antecedent of each branch label, so the
//!   skip takes only a path with no 2+ branch label, which ends at M, at
//!   Unreachable or at a loop hit.
//!
//! `GOPORT_FLOWSKIP`: unset or `1` on, `0` off, `verify` indexes every file,
//! tests each walk, then runs Go's walk anyway, and panics when the skip
//! would differ (result, counters of made things and effects, the planned
//! writes, visited nodes, depth). `GOPORT_FLOWSKIP_STATS=<file>` writes the
//! process totals of the tests to `<file>` (updated every 256 tests per
//! checker), with the time of the walks that the test refused, by reason.
//! `GOPORT_FLOWSKIP_BUILD_STEPS` and `GOPORT_FLOWSKIP_MIN_STEPS` set the
//! tuning values for A/B runs.

use crate::prelude::*;
use smallvec::SmallVec;
use std::cell::Cell;
use std::hash::Hasher;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

const NONE: u32 = u32::MAX;

/// Reference kinds (go-model-fix.md "Class").
const K_IDENT: u8 = 0;
const K_THIS: u8 = 1;
/// An access chain on an identifier root.
const K_ACC_IDENT: u8 = 2;
/// An access chain on `this`.
const K_ACC_THIS: u8 = 3;
const KINDS: usize = 4;

/// Mention and chain-site families. A `_U` family comes from a
/// discriminant role (`getDiscriminantPropertyAccess`, flow.go:1436), which
/// runs only when the declared type is a union.
const F_IDENT: u8 = 1;
const F_IDENT_U: u8 = 2;
const F_ACC: u8 = 3;
const F_ACC_U: u8 = 4;
/// Matched against each proper prefix of an access reference
/// (`containsMatchingReference`, flow.go:1841, and the `in` operand,
/// flow.go:523).
const F_ACCP: u8 = 5;
/// Chain sites: the root identifier of an access target or source whose
/// names equal the names of R (`C_M`, `C_M_U`) or of a proper access prefix
/// of R (`C_CM`) is resolved by Go (flow.go:1625-1633).
const C_M: u8 = 6;
const C_M_U: u8 = 7;
const C_CM: u8 = 8;

/// `c.inlineLevel < 5` (flow.go:386).
const MAX_INLINE: u8 = 5;

/// Root kinds of the next-pointer forest.
const ROOT_START: u8 = 1;
const ROOT_UNREACHABLE: u8 = 2;
const ROOT_BLOCKED: u8 = 3;
/// A loop label with 2+ antecedents (flow.go:1325): the walk reads the
/// loop cache there and goes on at the entry (`antecedents[0]`).
const ROOT_LOOP: u8 = 4;

/// Go's limit of nested `getTypeAtFlowNode` calls (flow.go:118).
const MAX_NEST: u32 = 2000;

/// Candidate states per class.
const S_UNKNOWN: u8 = 0;
const S_JOINED: u8 = 1;
const S_EXTRAS: u8 = 2;
const S_BLOCKED: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowSkipMode {
    Off,
    On,
    Verify,
}

/// `GOPORT_FLOWSKIP`, read once per process.
pub fn flow_skip_mode_from_env() -> FlowSkipMode {
    static MODE: OnceLock<FlowSkipMode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("GOPORT_FLOWSKIP").as_deref() {
        Ok("0") => FlowSkipMode::Off,
        Ok("verify") => FlowSkipMode::Verify,
        _ => FlowSkipMode::On,
    })
}

/// A tuning value from the environment (read once per process), else
/// `default`.
fn tuning(name: &'static str, default: u32) -> u32 {
    static VALUES: OnceLock<FxHashMap<&'static str, u32>> = OnceLock::new();
    let values = VALUES.get_or_init(|| {
        [
            "GOPORT_FLOWSKIP_BUILD_STEPS",
            "GOPORT_FLOWSKIP_MIN_STEPS",
            "GOPORT_FLOWSKIP_MIN_STEPS_M",
        ]
        .into_iter()
        .filter_map(|n| Some((n, std::env::var(n).ok()?.parse().ok()?)))
        .collect()
    });
    values.get(name).copied().unwrap_or(default)
}

/// Why a test said no (stats only).
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
enum Refusal {
    Kind,
    Inline,
    File,
    Counting,
    Short,
    Unreached,
    Mention,
    Chain,
    Depth,
    Unsettled,
    Blocked,
    Extras,
    Root,
    Cross,
    /// A loop cache entry of the key that is not the declared type.
    LoopOther,
    /// The key is on `flowLoopStack` with types (flow.go:1347).
    LoopStack,
    /// The rule at M (go-model-2-fix.md "At M") does not give declared.
    DeclRule,
    /// The dry run of `isReachableFlowNode(M)` aborts or gives false.
    DeclReach,
    /// Variant B: a 2+ branch label, a Start, no M, or a declared type
    /// that the label arithmetic would change.
    Region,
    /// Not tested: declared != initial and the reference is not an
    /// identifier (or Variant B is off).
    Initial,
}
const REFUSALS: usize = 20;
const REFUSAL_NAMES: [&str; REFUSALS] = [
    "kind",
    "inline",
    "file",
    "counting",
    "short",
    "unreached",
    "mention",
    "chain",
    "depth",
    "unsettled",
    "blocked",
    "extras",
    "root",
    "cross",
    "loop_other",
    "loop_stack",
    "decl_rule",
    "decl_reach",
    "region",
    "initial",
];

/// Counters of the tests of one checker (`GOPORT_FLOWSKIP_STATS`).
#[derive(Clone, Copy, Debug, Default)]
pub struct FlowSkipStats {
    pub tested: u64,
    pub skipped: u64,
    pub skipped_steps: u64,
    refused: [u64; REFUSALS],
    pub built: u64,
    pub built_nodes: u64,
    pub build_ns: u64,
    /// Nanoseconds of Go's walks after a refusal, by reason.
    refused_ns: [u64; REFUSALS],
    /// Walks whose result was the declared type, and their nanoseconds,
    /// by refusal reason (the walks a better rule could skip).
    refused_decl: [u64; REFUSALS],
    refused_decl_ns: [u64; REFUSALS],
    /// Nanoseconds of the tests (skipped or not).
    pub test_ns: u64,
    /// Skips that reached M (Variant A and B), skips of Variant B, skips
    /// that crossed a loop label, and dry runs of `isReachableFlowNode`.
    pub skipped_m: u64,
    pub skipped_b: u64,
    pub skipped_loop: u64,
    pub dry_runs: u64,
    pub dry_ns: u64,
    /// Per kind of skip (`Plan::category`): skips, nanoseconds of their
    /// tests, and (verify mode) nanoseconds of Go's walks of them.
    skip_count: [u64; 3],
    skip_test_ns: [u64; 3],
    skip_walk_ns: [u64; 3],
}

/// Skip kinds for the stats: Variant A with no M and no loop label,
/// Variant A with M or a loop label, Variant B.
const CATEGORY_NAMES: [&str; 3] = ["plain", "decl_or_loop", "variant_b"];

const STAT_BASE: usize = 24;
const STAT_SLOTS: usize = STAT_BASE + 4 * REFUSALS;
static STATS_TOTAL: [AtomicU64; STAT_SLOTS] = [const { AtomicU64::new(0) }; STAT_SLOTS];

fn stats_path() -> Option<&'static str> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var("GOPORT_FLOWSKIP_STATS").ok())
        .as_deref()
}

/// Whether `GOPORT_FLOWSKIP_STATS` is set (the walks are then timed).
fn stats_on() -> bool {
    stats_path().is_some()
}

impl FlowSkipStats {
    fn slots(&self) -> [u64; STAT_SLOTS] {
        let mut s = [0; STAT_SLOTS];
        s[0] = self.tested;
        s[1] = self.skipped;
        s[2] = self.skipped_steps;
        s[3] = self.built;
        s[4] = self.built_nodes;
        s[5] = self.build_ns;
        s[6] = self.test_ns;
        s[7] = self.skipped_m;
        s[8] = self.skipped_b;
        s[9] = self.skipped_loop;
        s[10] = self.dry_runs;
        s[11] = self.dry_ns;
        s[12..15].copy_from_slice(&self.skip_count);
        s[15..18].copy_from_slice(&self.skip_test_ns);
        s[18..21].copy_from_slice(&self.skip_walk_ns);
        let r = STAT_BASE;
        s[r..r + REFUSALS].copy_from_slice(&self.refused);
        s[r + REFUSALS..r + 2 * REFUSALS].copy_from_slice(&self.refused_ns);
        s[r + 2 * REFUSALS..r + 3 * REFUSALS].copy_from_slice(&self.refused_decl);
        s[r + 3 * REFUSALS..r + 4 * REFUSALS].copy_from_slice(&self.refused_decl_ns);
        s
    }

    /// Adds the counts since the last flush to the process totals and
    /// writes the totals to the stats file.
    fn flush(&mut self, flushed: &mut FlowSkipStats) {
        let Some(path) = stats_path() else {
            return;
        };
        let now = self.slots();
        let old = flushed.slots();
        for i in 0..STAT_SLOTS {
            STATS_TOTAL[i].fetch_add(now[i] - old[i], Ordering::Relaxed);
        }
        *flushed = *self;
        let total: Vec<u64> = STATS_TOTAL
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .collect();
        let mut text = format!(
            "tested {} skipped {} skipped_steps {} built {} built_nodes {} build_ms {}",
            total[0],
            total[1],
            total[2],
            total[3],
            total[4],
            total[5] / 1_000_000
        );
        let r = STAT_BASE;
        for (i, name) in REFUSAL_NAMES.iter().enumerate() {
            text.push_str(&format!(" {name} {}", total[r + i]));
        }
        text.push_str(&format!(
            "\ntest_ms {} skipped_m {} skipped_b {} skipped_loop {} dry_runs {} dry_ms {}\n",
            total[6] / 1_000_000,
            total[7],
            total[8],
            total[9],
            total[10],
            total[11] / 1_000_000
        ));
        // Per skip kind: skips, ms of their tests, ms of Go's walks of them
        // (verify mode only).
        for (i, name) in CATEGORY_NAMES.iter().enumerate() {
            text.push_str(&format!(
                "skip {name}: walks {} test_ms {:.1} walk_ms {:.1}\n",
                total[12 + i],
                total[15 + i] as f64 / 1e6,
                total[18 + i] as f64 / 1e6
            ));
        }
        // Per refusal: walks, ms of Go's walk, walks with the declared
        // result, their ms.
        for (i, name) in REFUSAL_NAMES.iter().enumerate() {
            if total[r + i] != 0 || total[r + REFUSALS + i] != 0 {
                text.push_str(&format!(
                    "{name}: walks {} ms {:.1} decl_walks {} decl_ms {:.1}\n",
                    total[r + i],
                    total[r + REFUSALS + i] as f64 / 1e6,
                    total[r + 2 * REFUSALS + i],
                    total[r + 3 * REFUSALS + i] as f64 / 1e6
                ));
            }
        }
        let _ = std::fs::write(path, text);
    }
}

/// Verify mode: what Go's walk did.
#[derive(Debug, Default)]
pub struct FlowSkipRecording {
    pub visited: Vec<FlowNodeId>,
    pub max_depth: i32,
}

/// The flow skip state of one checker (`Checker::flow_skip`).
pub struct FlowSkip {
    pub mode: FlowSkipMode,
    /// A file is indexed when the steps of its walks so far reach this many
    /// per flow node of the file (the index costs about as much as 10 walk
    /// steps per flow node, so a file with few or short walks would lose).
    /// Such files pay only the sample.
    pub build_steps: u32,
    /// A walk with fewer loop turns to its end (no move to an outer
    /// function, no loop entry) is not tested: Go's walk is cheaper than
    /// the test.
    pub min_steps: u32,
    /// The same for a walk that ends at M, whose test also reads the
    /// declaration and dry-runs `isReachableFlowNode`.
    pub min_steps_m: u32,
    /// Per file index (PERF, go-model-2.md 5.2: a `Vec`, not a hash map).
    files: Vec<FileState>,
    /// Port counter of effect sites, read by verify mode: effects signature
    /// misses, `ensureAssignmentsMarked` writes, `isExhaustiveSwitchStatement`
    /// computes, entity-name key resolutions.
    pub effects: u64,
    /// `isReachableFlowNode` calls at another node than `reach_target`
    /// (verify mode sets the target to the M that the skip planned).
    pub reach_off_target: u64,
    pub reach_target: FlowNodeId,
    /// Verify mode: set while Go's walk of a skipped reference runs.
    pub recording: Option<Box<FlowSkipRecording>>,
    pub stats: FlowSkipStats,
    stats_flushed: FlowSkipStats,
    /// Tests: each tested reference and whether the test skipped it.
    pub trace: Option<Vec<(Node, bool)>>,
    /// Final answers of `isConstantReference` for identifier symbols.
    constant_references: FxHashMap<SymbolId, bool>,
    /// The writes that the last passing test planned (scratch).
    plan: Plan,
    /// Scratch of the `isReachableFlowNode` dry run.
    dry_overlay: FxHashMap<FlowNodeId, bool>,
}

/// What Go's walk writes that the skip must write too, planned by a test
/// that passed (go-model-2.md 2.2 and 3).
#[derive(Default)]
struct Plan {
    /// The M that the walk reaches, or nil: Go's `isReachableFlowNode(M)`
    /// sets `lastFlowNode = M` and `lastFlowNodeReachable = true`.
    m: FlowNodeId,
    /// The `flowNodeReachable` entries that the dry run at M adds.
    reachable: Vec<(FlowNodeId, bool)>,
    /// The `flowLoopCache` keys that get the declared type.
    loops: Vec<FlowLoopKey>,
    /// The segments of the path, start and end (verify mode visits them
    /// in order).
    path_ends: Vec<(u32, u32)>,
    /// The nest sum of the path.
    nest: u32,
    /// The steps of the path (stats).
    steps: u32,
    /// Variant B (an identifier whose initial type is not declared).
    variant_b: bool,
}

impl Plan {
    /// The skip kind for the stats (`CATEGORY_NAMES`).
    fn category(&self) -> usize {
        if self.variant_b {
            2
        } else if self.m.is_some() || !self.loops.is_empty() {
            1
        } else {
            0
        }
    }

    fn clear(&mut self) {
        self.m = FlowNodeId::NIL;
        self.reachable.clear();
        self.loops.clear();
        self.path_ends.clear();
        self.nest = 0;
        self.steps = 0;
        self.variant_b = false;
    }
}

impl Default for FlowSkip {
    fn default() -> Self {
        let mode = flow_skip_mode_from_env();
        FlowSkip {
            mode,
            // Verify mode indexes every file at once, so it tests every walk.
            build_steps: tuning(
                "GOPORT_FLOWSKIP_BUILD_STEPS",
                if mode == FlowSkipMode::Verify { 0 } else { 16 },
            ),
            min_steps: tuning("GOPORT_FLOWSKIP_MIN_STEPS", 8),
            min_steps_m: tuning("GOPORT_FLOWSKIP_MIN_STEPS_M", 16),
            files: Vec::new(),
            effects: 0,
            reach_off_target: 0,
            reach_target: FlowNodeId::NIL,
            recording: None,
            stats: FlowSkipStats::default(),
            stats_flushed: FlowSkipStats::default(),
            trace: None,
            constant_references: FxHashMap::default(),
            plan: Plan::default(),
            dry_overlay: FxHashMap::default(),
        }
    }
}

impl std::fmt::Debug for FlowSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowSkip")
            .field("mode", &self.mode)
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}

enum FileState {
    /// Walks so far, and the estimated sum of their path lengths (one walk
    /// in `sample_every(walks)` is measured, at most `DRY_CAP` steps).
    Counting {
        walks: u32,
        steps: u64,
    },
    Built(Box<FileIndex>),
    /// Not a static file: never indexed.
    Unsupported,
}

/// A measured walk counts at most this many steps.
const DRY_CAP: u32 = 256;
/// One walk in this many is measured, and after `SAMPLE_BACKOFF` walks of
/// a file that is still below the build threshold, one in
/// `SAMPLE_EVERY_LATE` (go-model-2.md 5.2).
const SAMPLE_EVERY: u32 = 4;
const SAMPLE_BACKOFF: u32 = 4096;
const SAMPLE_EVERY_LATE: u32 = 32;

/// The sample step after `walks` walks of a file.
fn sample_every(walks: u32) -> u32 {
    if walks < SAMPLE_BACKOFF {
        SAMPLE_EVERY
    } else {
        SAMPLE_EVERY_LATE
    }
}

/// Steps of pi(u) to its root (no move to an outer function; a 2+ loop
/// label ends it, as its cache ends most of Go's walks there), at most
/// `DRY_CAP`: the length of Go's walk when nothing narrows.
fn dry_steps(flows: &[FlowNode], file: usize, u: usize) -> u32 {
    let mut x = u;
    let mut steps = 1;
    while steps < DRY_CAP {
        match next_of(flows, file, &flows[x]) {
            Ok((p, _)) => x = p as usize,
            Err(_) => break,
        }
        steps += 1;
    }
    steps
}

/// The entry (`antecedents[0]`) of the 2+ loop label `x`, when it is in
/// the file.
fn loop_entry(flows: &[FlowNode], file: usize, x: usize) -> Option<u32> {
    let id = *flows[x].antecedents.first()?;
    (id.is_some() && id.file_index() == file && id.local_index() < flows.len())
        .then_some(id.local_index() as u32)
}

// ──────────────────────────────────────────────────────────────────────
// Keys
// ──────────────────────────────────────────────────────────────────────

/// A mention key: family, the property names from the top of the chain
/// down, then the root text (`this` for the keyword). Equal texts give
/// equal keys; a hash collision only makes a test refuse more.
fn chain_key(family: u8, names: &[&'static str], root: &str) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    h.write_u8(family);
    for name in names {
        h.write(name.as_bytes());
        h.write_u8(0xff);
    }
    h.write_u8(0xfe);
    h.write(root.as_bytes());
    h.finish()
}

/// A chain-site key: family and the property names, without the root.
fn names_key(family: u8, names: &[&'static str]) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    h.write_u8(family);
    h.write_u8(0xfd);
    for name in names {
        h.write(name.as_bytes());
        h.write_u8(0xff);
    }
    h.finish()
}

fn root_text(root: Node) -> &'static str {
    if root.kind() == SyntaxKind::ThisKeyword {
        "this"
    } else {
        root.text()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Roles: the parts of `narrowType` (flow.go:377-1060) that a reference can
// meet when nothing matches it, as events.
// ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Ev {
    /// `isMatchingReference(R, x)` with x a target (flow.go:1597). The flag
    /// marks a discriminant role (union declared type only).
    M(Node, bool),
    /// `optionalChainContainsReference(x, R)` (flow.go:1851).
    Oc(Node),
    /// `containsMatchingReference(R, x)` (flow.go:1841), and the match of
    /// `R.Expression()` with the `in` operand (flow.go:523). Access kinds.
    Cm(Node),
    /// An identifier in an N position at alias depth d < 5: M, resolved
    /// (flow.go:388), inline alias candidate (flow.go:389-396), else T.
    NIdent(Node, bool, u8),
    /// An identifier in a discriminant position (union only): resolved,
    /// discriminant alias candidate (flow.go:1477-1491).
    DIdent(Node),
    /// Union-making `&&`/`||` (flow.go:539-551) or `switch (true)`
    /// (flow.go:1069).
    Block,
}

/// Go `getReferenceCandidate` (flow.go:1861), which only reads the tree.
fn reference_candidate(node: Node) -> Node {
    let mut node = node;
    loop {
        match node.kind() {
            SyntaxKind::ParenthesizedExpression => node = node.expression(),
            SyntaxKind::BinaryExpression => match node.operator_token().kind() {
                SyntaxKind::EqualsToken
                | SyntaxKind::BarBarEqualsToken
                | SyntaxKind::AmpersandAmpersandEqualsToken
                | SyntaxKind::QuestionQuestionEqualsToken => node = node.left(),
                SyntaxKind::CommaToken => node = node.right(),
                _ => return node,
            },
            _ => return node,
        }
    }
}

/// The target side of `isMatchingReference` (flow.go:1598-1604): Paren,
/// NonNull, the left of any assignment and the right of a comma.
fn unwrap_target(node: Node) -> Node {
    let mut node = node;
    loop {
        match node.kind() {
            SyntaxKind::ParenthesizedExpression | SyntaxKind::NonNullExpression => {
                node = node.expression();
            }
            SyntaxKind::BinaryExpression => {
                if is_assignment_expression(node, false) {
                    node = node.left();
                } else if node.operator_token().kind() == SyntaxKind::CommaToken {
                    node = node.right();
                } else {
                    return node;
                }
            }
            _ => return node,
        }
    }
}

/// The source side of `isMatchingReference` (flow.go:1620-1621, 1645-1646):
/// Paren, NonNull, Satisfies and the right of a comma.
fn unwrap_source(node: Node) -> Node {
    let mut node = node;
    loop {
        match node.kind() {
            SyntaxKind::ParenthesizedExpression
            | SyntaxKind::NonNullExpression
            | SyntaxKind::SatisfiesExpression => node = node.expression(),
            SyntaxKind::BinaryExpression
                if node.operator_token().kind() == SyntaxKind::CommaToken =>
            {
                node = node.right();
            }
            _ => return node,
        }
    }
}

/// An access chain as Go's recursion in `isMatchingReference` reads it.
enum Chain {
    /// Names from the top down, and the root (an identifier or `this`).
    Ok(SmallVec<[&'static str; 4]>, Node),
    /// An element access with an entity name argument: Go resolves the key
    /// (`tryGetNameFromEntityNameExpression`, flow.go:1753).
    KeySite,
    /// No reference of the port's kinds can match it, and nothing below is
    /// evaluated.
    None,
}

fn read_chain(node: Node, source: bool) -> Chain {
    let mut names = SmallVec::new();
    let mut x = node;
    loop {
        x = if source {
            unwrap_source(x)
        } else {
            unwrap_target(x)
        };
        match x.kind() {
            SyntaxKind::PropertyAccessExpression => {
                names.push(x.name().text());
                x = x.expression();
            }
            SyntaxKind::ElementAccessExpression => {
                let arg = x.argument_expression();
                if is_string_or_numeric_literal_like(arg) {
                    names.push(arg.text());
                    x = x.expression();
                } else if is_entity_name_expression(arg) {
                    return Chain::KeySite;
                } else {
                    return Chain::None;
                }
            }
            SyntaxKind::Identifier | SyntaxKind::ThisKeyword => return Chain::Ok(names, x),
            _ => return Chain::None,
        }
    }
}

/// Collects the events of one flow node or alias initializer.
struct Scan {
    strict: bool,
    out: Vec<Ev>,
}

impl Scan {
    /// Go `narrowType` (flow.go:377) on `e` at alias depth `d`.
    fn n(&mut self, e: Node, at: bool, d: u8) {
        if is_expression_of_optional_chain_root(e) || {
            let (parent, parent_kind) = node_parent_and_kind(e);
            parent_kind == SyntaxKind::BinaryExpression
                && matches!(
                    parent.operator_token().kind(),
                    SyntaxKind::QuestionQuestionToken | SyntaxKind::QuestionQuestionEqualsToken
                )
                && parent.left() == e
        } {
            // narrowTypeByOptionality (flow.go:415)
            self.out.push(Ev::M(e, false));
            self.d(e);
            return;
        }
        match e.kind() {
            SyntaxKind::Identifier => {
                if d < MAX_INLINE {
                    self.out.push(Ev::NIdent(e, at, d));
                } else {
                    self.t(e, at);
                }
            }
            SyntaxKind::ThisKeyword
            | SyntaxKind::SuperKeyword
            | SyntaxKind::PropertyAccessExpression
            | SyntaxKind::ElementAccessExpression => self.t(e, at),
            SyntaxKind::CallExpression => self.call(e),
            SyntaxKind::ParenthesizedExpression
            | SyntaxKind::NonNullExpression
            | SyntaxKind::SatisfiesExpression => self.n(e.expression(), at, d),
            SyntaxKind::BinaryExpression => self.binary(e, at, d),
            SyntaxKind::PrefixUnaryExpression if e.operator() == SyntaxKind::ExclamationToken => {
                self.n(e.operand(), !at, d);
            }
            _ => {}
        }
    }

    /// Go `narrowTypeByTruthiness` (flow.go:428).
    fn t(&mut self, e: Node, at: bool) {
        self.out.push(Ev::M(e, false));
        if self.strict && at {
            self.out.push(Ev::Oc(e));
        }
        self.d(e);
    }

    /// Go `getCandidateDiscriminantPropertyAccess` (flow.go:1457) for an
    /// ordinary reference.
    fn d(&mut self, x: Node) {
        if is_access_expression(x) {
            self.out.push(Ev::M(x.expression(), true));
        } else if is_identifier(x) {
            self.out.push(Ev::DIdent(x));
        }
    }

    /// Go `narrowTypeByCallExpression` (flow.go:444) with
    /// `hasMatchingArgument` (flow.go:1886). The `hasOwnProperty` match of
    /// `R.Expression()` (flow.go:459) is part of the CM site of the callee
    /// object: `getReferenceCandidate` strips a subset of what the target
    /// side of `isMatchingReference` strips.
    fn call(&mut self, call: Node) {
        for argument in call.arguments() {
            self.out.push(Ev::M(argument, false));
            self.out.push(Ev::Cm(argument));
            self.out.push(Ev::Oc(argument));
        }
        let callee = call.expression();
        if is_property_access_expression(callee) {
            let object = callee.expression();
            self.out.push(Ev::M(object, false));
            self.out.push(Ev::Cm(object));
        }
    }

    /// Go `narrowTypeByBinaryExpression` (flow.go:469).
    fn binary(&mut self, e: Node, at: bool, d: u8) {
        let operator = e.operator_token().kind();
        match operator {
            SyntaxKind::EqualsToken
            | SyntaxKind::BarBarEqualsToken
            | SyntaxKind::AmpersandAmpersandEqualsToken
            | SyntaxKind::QuestionQuestionEqualsToken => {
                self.n(e.right(), at, d);
                self.t(e.left(), at);
            }
            SyntaxKind::EqualsEqualsToken
            | SyntaxKind::ExclamationEqualsToken
            | SyntaxKind::EqualsEqualsEqualsToken
            | SyntaxKind::ExclamationEqualsEqualsToken => {
                let left = reference_candidate(e.left());
                let right = reference_candidate(e.right());
                if left.kind() == SyntaxKind::TypeOfExpression && is_string_literal_like(right) {
                    self.typeof_(left);
                    return;
                }
                if right.kind() == SyntaxKind::TypeOfExpression && is_string_literal_like(left) {
                    self.typeof_(right);
                    return;
                }
                self.out.push(Ev::M(left, false));
                self.out.push(Ev::M(right, false));
                if self.strict {
                    self.out.push(Ev::Oc(left));
                    self.out.push(Ev::Oc(right));
                }
                self.d(left);
                self.d(right);
                // isMatchingConstructorReference (flow.go:750)
                for side in [left, right] {
                    let name = if is_property_access_expression(side) {
                        side.name()
                    } else if is_element_access_expression(side)
                        && is_string_literal_like(side.argument_expression())
                    {
                        side.argument_expression()
                    } else {
                        Node::NIL
                    };
                    if name.is_some() && name.text() == "constructor" {
                        self.out.push(Ev::M(side.expression(), false));
                    }
                }
                // narrowTypeByBooleanComparison (flow.go:800)
                let (expr, bool_value) = if is_boolean_literal(right) && !is_access_expression(left)
                {
                    (left, right)
                } else if is_boolean_literal(left) && !is_access_expression(right) {
                    (right, left)
                } else {
                    return;
                };
                let at = (at != (bool_value.kind() == SyntaxKind::TrueKeyword))
                    != (operator != SyntaxKind::ExclamationEqualsEqualsToken
                        && operator != SyntaxKind::ExclamationEqualsToken);
                self.n(expr, at, d);
            }
            SyntaxKind::InstanceOfKeyword => {
                // narrowTypeByInstanceof (flow.go:811)
                let left = reference_candidate(e.left());
                self.out.push(Ev::M(left, false));
                if at && self.strict {
                    self.out.push(Ev::Oc(left));
                }
            }
            SyntaxKind::InKeyword => {
                let target = reference_candidate(e.right());
                if !is_private_identifier(e.left()) {
                    // flow.go:523, R.Expression() against the operand.
                    self.out.push(Ev::Cm(target));
                }
                self.out.push(Ev::M(target, false));
            }
            SyntaxKind::CommaToken => self.n(e.right(), at, d),
            SyntaxKind::AmpersandAmpersandToken => {
                self.n(e.left(), at, d);
                self.n(e.right(), at, d);
                if !at {
                    self.out.push(Ev::Block);
                }
            }
            SyntaxKind::BarBarToken => {
                self.n(e.left(), at, d);
                self.n(e.right(), at, d);
                if at {
                    self.out.push(Ev::Block);
                }
            }
            _ => {}
        }
    }

    /// Go `narrowTypeByTypeof` (flow.go:614) when the operand does not match.
    fn typeof_(&mut self, type_of_expr: Node) {
        let target = reference_candidate(type_of_expr.expression());
        self.out.push(Ev::M(target, false));
        if self.strict {
            self.out.push(Ev::Oc(target));
        }
        self.d(target);
    }

    /// Go `getTypeAtSwitchClause` (flow.go:1059) after the walk of the
    /// antecedent.
    fn switch(&mut self, switch_statement: Node) {
        let expr = skip_parentheses(switch_statement.expression());
        self.out.push(Ev::M(expr, false));
        let is_type_of = expr.kind() == SyntaxKind::TypeOfExpression;
        if is_type_of {
            self.out.push(Ev::M(expr.expression(), false));
        }
        if expr.kind() == SyntaxKind::TrueKeyword {
            self.out.push(Ev::Block);
            return;
        }
        if self.strict {
            self.out.push(Ev::Oc(expr));
            if is_type_of {
                self.out.push(Ev::Oc(expr.expression()));
            }
        }
        self.d(expr);
    }

    /// Go `getTypeAtFlowAssignment` (flow.go:220) when nothing matches.
    fn assignment(&mut self, node: Node) {
        self.out.push(Ev::M(node, false));
        self.out.push(Ev::Cm(node));
        if node.kind() == SyntaxKind::VariableDeclaration
            && is_for_in_statement(node.parent().parent())
        {
            let expr = node.parent().parent().expression();
            self.out.push(Ev::M(expr, false));
            self.out.push(Ev::Oc(expr));
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Meaning of the events for one reference kind
// ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Alias {
    /// N-position identifier: x, assumeTrue, alias depth.
    N(Node, bool, u8),
    /// D-position identifier.
    D(Node),
}

/// Bit of a resolve list or block for kind `k`; `u` marks the part that
/// runs only for a union declared type.
const fn slot(k: u8, u: bool) -> u8 {
    1 << (k * 2 + u as u8)
}
const ALL_BASE: u8 = 0b0101_0101;
const ALL_UNION: u8 = 0b1010_1010;
const ACC_BASE: u8 = slot(K_ACC_IDENT, false) | slot(K_ACC_THIS, false);

/// What the events of a node mean. Mention keys and chain sites carry
/// their family, which names the kinds that compare them, so they are kept
/// for every kind. Resolves and blocks carry the bits of the kinds (and
/// union part) that Go evaluates them for.
#[derive(Default)]
struct Out {
    mentions: Vec<u64>,
    chains: Vec<(u64, Node)>,
    resolves: Vec<(Node, u8)>,
    blocked: u8,
    aliases: Vec<Alias>,
}

impl Out {
    fn clear(&mut self) {
        self.mentions.clear();
        self.chains.clear();
        self.resolves.clear();
        self.blocked = 0;
        self.aliases.clear();
    }

    /// An access chain met by an access reference: its mention, and its
    /// chain site when its root is an identifier.
    fn chain(&mut self, chain: Chain, family: u8, site: u8, blocks: u8) {
        match chain {
            Chain::KeySite => self.blocked |= blocks,
            Chain::Ok(names, root) => {
                self.mentions
                    .push(chain_key(family, &names, root_text(root)));
                if root.kind() == SyntaxKind::Identifier && !names.is_empty() {
                    self.chains.push((names_key(site, &names), root));
                }
            }
            Chain::None => {}
        }
    }

    /// `isMatchingReference(R, x)`, x a target (flow.go:1597): an
    /// identifier reference resolves an identifier target; an access
    /// reference reads the names of an access target.
    fn m(&mut self, x: Node, u: bool) {
        let t = unwrap_target(x);
        let family = if u { F_IDENT_U } else { F_IDENT };
        match t.kind() {
            SyntaxKind::Identifier => {
                self.mentions.push(chain_key(family, &[], t.text()));
                self.resolves.push((t, slot(K_IDENT, u)));
            }
            SyntaxKind::VariableDeclaration | SyntaxKind::BindingElement => {
                // A hole of an array binding pattern is a binding element
                // with no name.
                let name = t.name();
                if name.is_some() && is_identifier(name) {
                    self.mentions.push(chain_key(family, &[], name.text()));
                }
            }
            SyntaxKind::ThisKeyword => self.mentions.push(chain_key(family, &[], "this")),
            SyntaxKind::PropertyAccessExpression | SyntaxKind::ElementAccessExpression => {
                let (family, site) = if u { (F_ACC_U, C_M_U) } else { (F_ACC, C_M) };
                self.chain(
                    read_chain(t, false),
                    family,
                    site,
                    slot(K_ACC_IDENT, u) | slot(K_ACC_THIS, u),
                );
            }
            _ => {}
        }
    }

    /// `optionalChainContainsReference(x, R)` (flow.go:1851): each prefix
    /// of the chain is a source of `isMatchingReference` with R as the
    /// target.
    fn oc(&mut self, x: Node) {
        let mut s = x;
        while is_optional_chain(s) {
            s = s.expression();
            let src = unwrap_source(s);
            match src.kind() {
                SyntaxKind::Identifier => {
                    if is_this_in_type_query(src) {
                        self.mentions.push(chain_key(F_IDENT, &[], "this"));
                    } else {
                        self.mentions.push(chain_key(F_IDENT, &[], src.text()));
                        self.resolves.push((src, slot(K_IDENT, false)));
                    }
                }
                SyntaxKind::ThisKeyword => self.mentions.push(chain_key(F_IDENT, &[], "this")),
                SyntaxKind::PropertyAccessExpression | SyntaxKind::ElementAccessExpression => {
                    // The port reads the name of an element access source
                    // before it looks at the target (flow_p2.rs
                    // is_matching_reference_kind), for every kind.
                    if src.kind() == SyntaxKind::ElementAccessExpression
                        && is_entity_name_expression(src.argument_expression())
                    {
                        self.blocked |= ALL_BASE;
                    } else {
                        self.chain(read_chain(src, true), F_ACC, C_M, ACC_BASE);
                    }
                }
                _ => {}
            }
        }
    }

    /// `containsMatchingReference(R, x)` (flow.go:1841): each proper prefix
    /// of an access reference is a source with x as the target.
    fn cm(&mut self, x: Node) {
        let t = unwrap_target(x);
        match t.kind() {
            SyntaxKind::Identifier => {
                self.mentions.push(chain_key(F_ACCP, &[], t.text()));
                self.resolves.push((t, slot(K_ACC_IDENT, false)));
            }
            SyntaxKind::ThisKeyword => self.mentions.push(chain_key(F_ACCP, &[], "this")),
            SyntaxKind::VariableDeclaration | SyntaxKind::BindingElement => {
                let name = t.name();
                if name.is_some() && is_identifier(name) {
                    self.mentions.push(chain_key(F_ACCP, &[], name.text()));
                }
            }
            SyntaxKind::PropertyAccessExpression | SyntaxKind::ElementAccessExpression => {
                self.chain(read_chain(t, false), F_ACCP, C_CM, ACC_BASE);
            }
            _ => {}
        }
    }

    fn event(&mut self, ev: Ev) {
        match ev {
            Ev::M(x, u) => self.m(x, u),
            Ev::Oc(x) => self.oc(x),
            Ev::Cm(x) => self.cm(x),
            Ev::NIdent(x, at, d) => {
                self.m(x, false);
                // narrowType resolves it for every kind (flow.go:388).
                self.resolves.push((x, ALL_BASE));
                self.aliases.push(Alias::N(x, at, d));
            }
            Ev::DIdent(x) => {
                self.resolves.push((x, ALL_UNION));
                self.aliases.push(Alias::D(x));
            }
            Ev::Block => self.blocked |= ALL_BASE,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// The per-file index
// ──────────────────────────────────────────────────────────────────────

struct Cand {
    node: u32,
    /// The call of a call node, else nil.
    call: Node,
    /// `res_pool[res_start..res_end]`: identifiers Go resolves here, with
    /// the bits (`slot`) of the kinds that resolve them.
    res_start: u32,
    res_end: u32,
    /// Bits (`slot`): a key site or union-making operator.
    blocked: u8,
    alias_start: u32,
    alias_end: u32,
}

/// Extras of a settled candidate for one class: mentions and chain sites
/// that its aliases add, checked per walk.
#[derive(Default)]
struct Extras {
    mentions: Vec<u64>,
    chains: Vec<(u64, Node)>,
}

struct ClassState {
    next: Vec<u32>,
    state: Vec<u8>,
    extras: FxHashMap<u32, Extras>,
}

struct FileIndex {
    flows: &'static [FlowNode],
    file: usize,
    parent: Vec<u32>,
    root_kind: Vec<u8>,
    nest: Vec<u32>,
    /// 2+ branch labels on the path from each node to its root.
    labels: Vec<u32>,
    steps: Vec<u32>,
    tin: Vec<u32>,
    tout: Vec<u32>,
    stop: Vec<u32>,
    /// The root of each reached node's tree.
    root_of: Vec<u32>,
    cand_of: Vec<u32>,
    cands: Vec<Cand>,
    res_pool: Vec<(Node, u8)>,
    alias_pool: Vec<Alias>,
    /// Mentions: `(key, tin)` and `(key, tout)` of each mentioning node,
    /// sorted; `mention_range[key]` is the part of both lists with `key`.
    mention_in: Vec<u32>,
    mention_out: Vec<u32>,
    mention_range: FxHashMap<u64, (u32, u32)>,
    /// Chain sites sorted by key, then tin: key, tin, tout, root.
    chain_sites: Vec<(u64, u32, u32, Node)>,
    /// `chain_range[key]`: the part of `chain_sites` with `key`.
    chain_range: FxHashMap<u64, (u32, u32)>,
    /// The nearest site of the same key that encloses each site, or NONE.
    chain_up: Vec<u32>,
    /// Per checker: a union-find over the sites. A site whose root was
    /// found resolved points to `chain_up` (a resolved root stays
    /// resolved); `chain_sites.len()` is "no site".
    chain_skip: Vec<u32>,
    /// The Assignment flow node of each VariableDeclaration or
    /// BindingElement (NONE when two flow nodes have it).
    decl_flow: FxHashMap<Node, u32>,
    classes: Vec<Option<Box<ClassState>>>,
}

#[derive(Clone, Copy, Debug)]
struct Class {
    kind: u8,
    union: bool,
    /// `isConstantReference(R)`: 0 false, 1 true, 2 unknown (not pure yet).
    cr: u8,
}

impl Class {
    fn index(self) -> usize {
        usize::from(self.kind) * 6 + usize::from(self.union) * 3 + usize::from(self.cr)
    }
}

const CLASSES: usize = KINDS * 6;

/// Static next pointer of a flow node: the node Go's walk visits after it
/// when the node is inert (pi in go-model.md 2.2), and whether that step
/// nests. A root has kind `ROOT_*`.
fn next_of(flows: &[FlowNode], file: usize, flow: &FlowNode) -> Result<(u32, u32), u8> {
    let flags = flow.flags;
    let local = |id: FlowNodeId| -> Result<u32, u8> {
        if id.is_nil() || id.file_index() != file || id.local_index() >= flows.len() {
            Err(ROOT_BLOCKED)
        } else {
            Ok(id.local_index() as u32)
        }
    };
    if flags.intersects(FlowFlags::ASSIGNMENT | FlowFlags::CALL) {
        Ok((local(flow.antecedent)?, 0))
    } else if flags.intersects(FlowFlags::CONDITION | FlowFlags::SWITCH_CLAUSE) {
        Ok((local(flow.antecedent)?, 1))
    } else if flags.intersects(FlowFlags::BRANCH_LABEL) {
        match flow.antecedents.len() {
            0 => Err(ROOT_BLOCKED),
            1 => Ok((local(flow.antecedents[0])?, 0)),
            _ => {
                let first = local(flow.antecedents[0])?;
                let first_flow = &flows[first as usize];
                // The first empty switch clause is the bypass (flow.go:1260).
                let bypass = first_flow.flags.intersects(FlowFlags::SWITCH_CLAUSE)
                    && first_flow.antecedents[0].0 as u32 == first_flow.antecedents[1].0 as u32;
                if bypass {
                    Ok((local(flow.antecedents[1])?, 1))
                } else {
                    Ok((first, 1))
                }
            }
        }
    } else if flags.intersects(FlowFlags::LOOP_LABEL) {
        // A loop label with 2+ antecedents reads and writes `flowLoopCache`
        // (flow.go:1325): a root of its own kind; the walk goes on at its
        // entry when the cache misses.
        match flow.antecedents.len() {
            0 => Err(ROOT_BLOCKED),
            1 => Ok((local(flow.antecedents[0])?, 0)),
            _ => Err(ROOT_LOOP),
        }
    } else if flags.intersects(FlowFlags::ARRAY_MUTATION) {
        Ok((local(flow.antecedent)?, 0))
    } else if flags.intersects(FlowFlags::REDUCE_LABEL) {
        Err(ROOT_BLOCKED)
    } else if flags.intersects(FlowFlags::START) {
        Err(ROOT_START)
    } else {
        Err(ROOT_UNREACHABLE)
    }
}

fn is_candidate(flags: FlowFlags) -> bool {
    // Same test order as `get_type_at_flow_node`.
    flags.intersects(FlowFlags::ASSIGNMENT | FlowFlags::CALL)
        || flags.intersects(FlowFlags::CONDITION | FlowFlags::SWITCH_CLAUSE)
}

/// The dry run of `isReachableFlowNode` aborts deeper than this.
const MAX_DRY_DEPTH: u32 = 4096;

/// A loop label read before the entry walk (flow.go:1336-1351).
enum LoopRead {
    /// The cache holds the declared type: the walk ends at the label.
    Hit,
    /// No entry and no stack entry with types: Go walks the entry and
    /// writes the declared type.
    Miss,
}

/// Go `isFalseExpression` (flow.go:2589), which reads only the tree.
fn is_false_expression_pure(expr: Node) -> bool {
    let node = skip_parentheses(expr);
    if node.kind() == SyntaxKind::FalseKeyword {
        return true;
    }
    if is_binary_expression(node) {
        let operator = node.operator_token().kind();
        return operator == SyntaxKind::AmpersandAmpersandToken
            && (is_false_expression_pure(node.left()) || is_false_expression_pure(node.right()))
            || operator == SyntaxKind::BarBarToken
                && is_false_expression_pure(node.left())
                && is_false_expression_pure(node.right());
    }
    false
}

impl FileIndex {
    fn build(file: usize, flows: &'static [FlowNode], strict: bool) -> FileIndex {
        let n = flows.len();
        let mut parent = vec![NONE; n];
        let mut root_kind = vec![0u8; n];
        let mut nest_self = vec![0u32; n];
        let mut cand_of = vec![NONE; n];
        let mut cands: Vec<Cand> = Vec::new();
        for (i, flow) in flows.iter().enumerate() {
            match next_of(flows, file, flow) {
                Ok((p, nest)) => {
                    parent[i] = p;
                    nest_self[i] = nest;
                    if is_candidate(flow.flags) {
                        cand_of[i] = cands.len() as u32;
                        cands.push(Cand {
                            node: i as u32,
                            call: Node::NIL,
                            res_start: 0,
                            res_end: 0,
                            blocked: 0,
                            alias_start: 0,
                            alias_end: 0,
                        });
                    }
                }
                Err(kind) => root_kind[i] = kind,
            }
        }
        // Children lists (CSR), then a DFS from the roots. Nodes on a cycle
        // (a loop label whose only antecedent is a back edge) stay
        // unreached: no skip there.
        let mut child_start = vec![0u32; n + 1];
        for &p in &parent {
            if p != NONE {
                child_start[p as usize + 1] += 1;
            }
        }
        for i in 0..n {
            child_start[i + 1] += child_start[i];
        }
        let mut fill = child_start.clone();
        let mut children = vec![0u32; child_start[n] as usize];
        for (i, &p) in parent.iter().enumerate() {
            if p != NONE {
                children[fill[p as usize] as usize] = i as u32;
                fill[p as usize] += 1;
            }
        }
        drop(fill);
        let mut tin = vec![NONE; n];
        let mut tout = vec![NONE; n];
        let mut nest = vec![0u32; n];
        // 2+ branch labels on the path to the root (Variant B walks every
        // antecedent there, so a path with none is the whole region).
        let mut labels = vec![0u32; n];
        let mut steps = vec![0u32; n];
        let mut stop = vec![NONE; n];
        let mut root_of = vec![NONE; n];
        let mut counter = 0u32;
        let mut stack: Vec<(u32, u32)> = Vec::new();
        for r in 0..n {
            if root_kind[r] == 0 {
                continue;
            }
            tin[r] = counter;
            counter += 1;
            steps[r] = 1;
            stop[r] = r as u32;
            root_of[r] = r as u32;
            stack.push((r as u32, child_start[r]));
            while let Some(top) = stack.last_mut() {
                let (x, ci) = *top;
                if ci < child_start[x as usize + 1] {
                    top.1 += 1;
                    let c = children[ci as usize] as usize;
                    tin[c] = counter;
                    counter += 1;
                    nest[c] = nest_self[c] + nest[x as usize];
                    labels[c] = labels[x as usize]
                        + u32::from(
                            flows[c].flags.intersects(FlowFlags::BRANCH_LABEL)
                                && flows[c].antecedents.len() > 1,
                        );
                    steps[c] = steps[x as usize] + 1;
                    stop[c] = if cand_of[c] != NONE {
                        c as u32
                    } else {
                        stop[x as usize]
                    };
                    root_of[c] = root_of[x as usize];
                    stack.push((c as u32, child_start[c]));
                } else {
                    tout[x as usize] = counter;
                    stack.pop();
                }
            }
        }
        drop(children);
        drop(child_start);
        // The M node of each declaration (binder.go:2336-2357).
        let mut decl_flow: FxHashMap<Node, u32> = FxHashMap::default();
        for (i, flow) in flows.iter().enumerate() {
            if flow.flags.intersects(FlowFlags::ASSIGNMENT)
                && matches!(
                    flow.node.kind(),
                    SyntaxKind::VariableDeclaration | SyntaxKind::BindingElement
                )
            {
                decl_flow
                    .entry(flow.node)
                    .and_modify(|m| *m = NONE)
                    .or_insert(i as u32);
            }
        }
        // Events of the reached candidates.
        let mut res_pool = Vec::new();
        let mut alias_pool = Vec::new();
        let mut mention_in = Vec::new();
        let mut mention_out = Vec::new();
        let mut chain_sites = Vec::new();
        let mut scan = Scan {
            strict,
            out: Vec::new(),
        };
        let mut out = Out::default();
        for cand in &mut cands {
            let i = cand.node as usize;
            if tin[i] == NONE {
                continue;
            }
            let flow = &flows[i];
            scan.out.clear();
            let flags = flow.flags;
            if flags.intersects(FlowFlags::ASSIGNMENT) {
                scan.assignment(flow.node);
            } else if flags.intersects(FlowFlags::CALL) {
                cand.call = flow.node;
                continue;
            } else if flags.intersects(FlowFlags::CONDITION) {
                scan.n(flow.node, flags.intersects(FlowFlags::TRUE_CONDITION), 0);
            } else {
                scan.switch(flow.node);
            }
            out.clear();
            for &ev in &scan.out {
                out.event(ev);
            }
            cand.blocked = out.blocked;
            out.resolves.sort_unstable_by_key(|r| r.0);
            cand.res_start = res_pool.len() as u32;
            for &(x, bits) in &out.resolves {
                let merge = res_pool.len() as u32 > cand.res_start;
                match res_pool.last_mut() {
                    Some((last, last_bits)) if merge && *last == x => *last_bits |= bits,
                    _ => res_pool.push((x, bits)),
                }
            }
            cand.res_end = res_pool.len() as u32;
            cand.alias_start = alias_pool.len() as u32;
            alias_pool.extend_from_slice(&out.aliases);
            cand.alias_end = alias_pool.len() as u32;
            out.mentions.sort_unstable();
            out.mentions.dedup();
            for &key in &out.mentions {
                mention_in.push((key, tin[i]));
                mention_out.push((key, tout[i]));
            }
            out.chains.sort_unstable();
            out.chains.dedup();
            for &(key, root) in &out.chains {
                chain_sites.push((key, tin[i], tout[i], root));
            }
        }
        mention_in.sort_unstable();
        mention_out.sort_unstable();
        let mut mention_range = FxHashMap::default();
        let mut start = 0;
        while start < mention_in.len() {
            let key = mention_in[start].0;
            let mut end = start + 1;
            while end < mention_in.len() && mention_in[end].0 == key {
                end += 1;
            }
            mention_range.insert(key, (start as u32, end as u32));
            start = end;
        }
        let mention_in = mention_in.into_iter().map(|e| e.1).collect();
        let mention_out = mention_out.into_iter().map(|e| e.1).collect();
        // Chain sites by key, then tin; per key the nearest enclosing site
        // of the same key (one stack pass in tin order; on one flow node
        // the earlier site encloses the later).
        chain_sites.sort_unstable_by_key(|s: &(u64, u32, u32, Node)| (s.0, s.1));
        let mut chain_range = FxHashMap::default();
        let mut chain_up = vec![NONE; chain_sites.len()];
        let mut open: Vec<u32> = Vec::new();
        let mut start = 0;
        while start < chain_sites.len() {
            let key = chain_sites[start].0;
            let mut end = start;
            open.clear();
            while end < chain_sites.len() && chain_sites[end].0 == key {
                let site_tin = chain_sites[end].1;
                while let Some(&top) = open.last() {
                    if chain_sites[top as usize].2 <= site_tin {
                        open.pop();
                    } else {
                        break;
                    }
                }
                chain_up[end] = open.last().copied().unwrap_or(NONE);
                open.push(end as u32);
                end += 1;
            }
            chain_range.insert(key, (start as u32, end as u32));
            start = end;
        }
        let chain_skip = (0..=chain_sites.len() as u32).collect();
        FileIndex {
            flows,
            file,
            parent,
            root_kind,
            nest,
            labels,
            steps,
            tin,
            tout,
            stop,
            root_of,
            cand_of,
            cands,
            res_pool,
            alias_pool,
            mention_in,
            mention_out,
            mention_range,
            chain_sites,
            chain_range,
            chain_up,
            chain_skip,
            decl_flow,
            classes: (0..CLASSES).map(|_| None).collect(),
        }
    }

    /// Whether `a` is an ancestor of `b` or `b` itself in the forest.
    fn encloses(&self, a: usize, b: usize) -> bool {
        self.tin[a] <= self.tin[b] && self.tin[b] < self.tout[a]
    }

    /// Nodes on the path from the node with entry number `p` to its root
    /// that mention `key`.
    fn mentions_on_path(&self, key: u64, p: u32) -> usize {
        let Some(&(a, b)) = self.mention_range.get(&key) else {
            return 0;
        };
        let (a, b) = (a as usize, b as usize);
        self.mention_in[a..b].partition_point(|&t| t <= p)
            - self.mention_out[a..b].partition_point(|&t| t <= p)
    }

    fn class_state(&mut self, class: Class) -> &mut ClassState {
        let slot = &mut self.classes[class.index()];
        slot.get_or_insert_with(|| {
            Box::new(ClassState {
                next: self.stop.clone(),
                state: vec![S_UNKNOWN; self.cands.len()],
                extras: FxHashMap::default(),
            })
        })
    }
}

/// Union-find with path halving.
fn find(next: &mut [u32], mut x: u32) -> u32 {
    while next[x as usize] != x {
        let p = next[x as usize];
        next[x as usize] = next[p as usize];
        x = p;
    }
    x
}

/// The keys of a reference that the index compares.
struct RefKeys {
    kind: u8,
    /// The root identifier (or `this`).
    root: Node,
    /// Mention keys: a node that mentions one of them may match R.
    mentions: SmallVec<[u64; 8]>,
    /// Chain-site keys: a site with one of them needs a resolved root.
    chains: SmallVec<[u64; 4]>,
}

enum Settle {
    Joined,
    Extras(Extras),
    Unsettled,
    Blocked,
}

// ──────────────────────────────────────────────────────────────────────
// Checker side
// ──────────────────────────────────────────────────────────────────────

/// Values that a skipped walk must not change, other than the planned
/// writes (verify mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    types: u32,
    symbols: u32,
    signatures: u32,
    diagnostics: i32,
    suggestions: i32,
    union_of_unions: usize,
    assignment_reduced: usize,
    flow_loop_cache: usize,
    last_flow_node: FlowNodeId,
    last_flow_node_reachable: bool,
    flow_node_reachable: usize,
    flow_analysis_disabled: bool,
    flow_invocation_count: i32,
    effects: u64,
    reach_off_target: u64,
    resolve_names: u64,
}

impl Checker {
    fn flow_skip_resolved(&self, node: Node) -> bool {
        self.symbol_node_links
            .try_get(node)
            .is_some_and(|links| links.resolved_symbol.is_some())
    }

    /// Kind and keys of `reference`, or `None` when the kind is not one
    /// the index knows (P2 in go-model.md 2.4).
    fn flow_skip_ref_keys(&self, reference: Node, union: bool) -> Option<RefKeys> {
        let mut mentions = SmallVec::new();
        let mut chains = SmallVec::new();
        match reference.kind() {
            SyntaxKind::Identifier => {
                if is_this_in_type_query(reference) {
                    return None;
                }
                let symbol = self.symbol_node_links.try_get(reference)?.resolved_symbol;
                if symbol.is_nil() || symbol == self.unknown_symbol {
                    return None;
                }
                mentions.push(chain_key(F_IDENT, &[], reference.text()));
                if union {
                    mentions.push(chain_key(F_IDENT_U, &[], reference.text()));
                }
                Some(RefKeys {
                    kind: K_IDENT,
                    root: reference,
                    mentions,
                    chains,
                })
            }
            SyntaxKind::ThisKeyword => {
                mentions.push(chain_key(F_IDENT, &[], "this"));
                if union {
                    mentions.push(chain_key(F_IDENT_U, &[], "this"));
                }
                Some(RefKeys {
                    kind: K_THIS,
                    root: reference,
                    mentions,
                    chains,
                })
            }
            SyntaxKind::PropertyAccessExpression | SyntaxKind::ElementAccessExpression => {
                // A plain chain: no wrappers, literal element arguments.
                let mut names: SmallVec<[&'static str; 4]> = SmallVec::new();
                let mut x = reference;
                let root = loop {
                    match x.kind() {
                        SyntaxKind::PropertyAccessExpression => {
                            names.push(x.name().text());
                            x = x.expression();
                        }
                        SyntaxKind::ElementAccessExpression => {
                            let arg = x.argument_expression();
                            if !is_string_or_numeric_literal_like(arg) {
                                return None;
                            }
                            names.push(arg.text());
                            x = x.expression();
                        }
                        SyntaxKind::Identifier => {
                            if is_this_in_type_query(x) {
                                return None;
                            }
                            let symbol = self.symbol_node_links.try_get(x)?.resolved_symbol;
                            if symbol.is_nil() || symbol == self.unknown_symbol {
                                return None;
                            }
                            break x;
                        }
                        SyntaxKind::ThisKeyword => break x,
                        _ => return None,
                    }
                };
                let kind = if root.kind() == SyntaxKind::Identifier {
                    K_ACC_IDENT
                } else {
                    K_ACC_THIS
                };
                let root_node = root;
                let root = root_text(root);
                mentions.push(chain_key(F_ACC, &names, root));
                if union {
                    mentions.push(chain_key(F_ACC_U, &names, root));
                }
                for i in 1..=names.len() {
                    mentions.push(chain_key(F_ACCP, &names[i..], root));
                }
                if kind == K_ACC_IDENT {
                    chains.push(names_key(C_M, &names));
                    if union {
                        chains.push(names_key(C_M_U, &names));
                    }
                    for i in 1..names.len() {
                        chains.push(names_key(C_CM, &names[i..]));
                    }
                }
                Some(RefKeys {
                    kind,
                    root: root_node,
                    mentions,
                    chains,
                })
            }
            _ => None,
        }
    }

    /// Go `isConstantReference(R)` (flow.go:1814) when it is pure: `None`
    /// when Go's call would write state (go-model-fix.md item 4).
    fn flow_skip_constant_reference(&mut self, node: Node) -> Option<bool> {
        match node.kind() {
            SyntaxKind::ThisKeyword => Some(true),
            SyntaxKind::Identifier => {
                let symbol = self.symbol_node_links.try_get(node)?.resolved_symbol;
                if symbol.is_nil() {
                    return None;
                }
                if self.is_constant_variable(symbol) {
                    return Some(true);
                }
                if self.is_parameter_or_mutable_local_variable(symbol) {
                    // isSymbolAssigned: ensureAssignmentsMarked (flow.go:2674)
                    // writes nothing when its function is marked already.
                    let parent = find_ancestor(
                        self.sym(symbol).value_declaration,
                        is_function_or_source_file,
                    );
                    if parent.is_some()
                        && !self.node_links.try_get(parent).is_some_and(|links| {
                            links.flags.intersects(NodeCheckFlags::ASSIGNMENTS_MARKED)
                        })
                    {
                        return None;
                    }
                    let assigned = self
                        .marked_assignment_symbol_links
                        .try_get(symbol)
                        .is_some_and(|links| links.last_assignment_pos != 0);
                    if !assigned {
                        return Some(true);
                    }
                }
                let value_declaration = self.sym(symbol).value_declaration;
                Some(value_declaration.is_some() && is_function_expression(value_declaration))
            }
            SyntaxKind::PropertyAccessExpression | SyntaxKind::ElementAccessExpression => {
                if !self.flow_skip_constant_reference_cached(node.expression())? {
                    return Some(false);
                }
                let symbol = self
                    .symbol_node_links
                    .try_get(node)
                    .map_or(SymbolId::NIL, |links| links.resolved_symbol);
                if symbol.is_nil() {
                    return Some(false);
                }
                // isReadonlyAssignmentDeclaration checks expressions: not pure.
                if self
                    .sym(symbol)
                    .declarations
                    .iter()
                    .any(|&d| is_call_expression(d))
                {
                    return None;
                }
                Some(self.is_readonly_symbol(symbol))
            }
            _ => None,
        }
    }

    /// Entry from `get_flow_type_of_reference_ex` after `flowInvocationCount++`
    /// (flow.go:97): Go's walk of `reference`, skipped when the test passes.
    /// Out of line, so the caller stays small (go-model-2.md 5.2). The
    /// caller has checked P1 (no explicit flow node) and that declared is
    /// not auto (flow.go:232).
    #[inline(never)]
    pub(crate) fn flow_skip_walk(
        &mut self,
        reference: Node,
        declared_type: TypeId,
        initial_type: TypeId,
        flow_container: Node,
        flow: FlowNodeId,
    ) -> TypeId {
        // Go `f.initialType = Coalesce(initialType, declaredType)` (flow.go:94,
        // CE3). Variant A: equal to declared. Variant B: an identifier only.
        let initial = if initial_type.is_nil() {
            declared_type
        } else {
            initial_type
        };
        if initial != declared_type && reference.kind() != SyntaxKind::Identifier {
            if stats_on() {
                return self.flow_skip_timed_walk(
                    Refusal::Initial,
                    reference,
                    declared_type,
                    initial_type,
                    flow_container,
                    flow,
                );
            }
            return self.flow_walk(reference, declared_type, initial_type, flow_container, flow);
        }
        let start = stats_on().then(std::time::Instant::now);
        let result = self.flow_skip_test(reference, declared_type, initial, flow_container, flow);
        if let Some(start) = start {
            let ns = start.elapsed().as_nanos() as u64;
            let stats = &mut self.flow_skip.stats;
            stats.test_ns += ns;
            if result.is_ok() {
                let category = self.flow_skip.plan.category();
                stats.skip_count[category] += 1;
                stats.skip_test_ns[category] += ns;
            }
        }
        match result {
            Ok(()) => {
                if self.flow_skip.mode == FlowSkipMode::Verify {
                    self.flow_skip_verify(
                        reference,
                        declared_type,
                        initial_type,
                        flow_container,
                        flow,
                    )
                } else {
                    self.flow_skip_commit(declared_type);
                    declared_type
                }
            }
            Err(why) => {
                if stats_on() {
                    return self.flow_skip_timed_walk(
                        why,
                        reference,
                        declared_type,
                        initial_type,
                        flow_container,
                        flow,
                    );
                }
                self.flow_walk(reference, declared_type, initial_type, flow_container, flow)
            }
        }
    }

    /// Stats mode: Go's walk after refusal `why`, timed, and whether it
    /// gave the declared type.
    #[cold]
    #[inline(never)]
    fn flow_skip_timed_walk(
        &mut self,
        why: Refusal,
        reference: Node,
        declared_type: TypeId,
        initial_type: TypeId,
        flow_container: Node,
        flow: FlowNodeId,
    ) -> TypeId {
        let start = std::time::Instant::now();
        let result = self.flow_walk(reference, declared_type, initial_type, flow_container, flow);
        let ns = start.elapsed().as_nanos() as u64;
        let stats = &mut self.flow_skip.stats;
        let i = why as usize;
        if matches!(why, Refusal::Initial) {
            stats.refused[i] += 1;
        }
        stats.refused_ns[i] += ns;
        if result == declared_type {
            stats.refused_decl[i] += 1;
            stats.refused_decl_ns[i] += ns;
        }
        result
    }

    /// Writes what Go's walk would write (the plan of the test that passed).
    fn flow_skip_commit(&mut self, declared_type: TypeId) {
        let plan = std::mem::take(&mut self.flow_skip.plan);
        for &(flow, reachable) in &plan.reachable {
            self.flow_node_reachable.insert(flow, reachable);
        }
        if plan.m.is_some() {
            self.last_flow_node = plan.m;
            self.last_flow_node_reachable = true;
        }
        for &key in &plan.loops {
            self.flow_loop_cache.insert(key, declared_type);
        }
        self.flow_skip.plan = plan;
    }

    /// The flow skip test: true when Go's walk of `reference` from `flow`
    /// visits only inert nodes, returns `declared_type`, and writes only
    /// what `self.flow_skip.plan` holds after the test.
    fn flow_skip_test(
        &mut self,
        reference: Node,
        declared_type: TypeId,
        initial_type: TypeId,
        flow_container: Node,
        flow: FlowNodeId,
    ) -> Result<(), Refusal> {
        self.flow_skip.stats.tested += 1;
        if self.flow_skip.stats.tested & 255 == 0 {
            let mut flushed = self.flow_skip.stats_flushed;
            self.flow_skip.stats.flush(&mut flushed);
            self.flow_skip.stats_flushed = flushed;
        }
        let result = self.flow_skip_test_worker(
            reference,
            declared_type,
            initial_type,
            flow_container,
            flow,
        );
        if let Some(trace) = self.flow_skip.trace.as_mut() {
            trace.push((reference, result.is_ok()));
        }
        match result {
            Ok(()) => {
                let stats = &mut self.flow_skip.stats;
                let plan = &self.flow_skip.plan;
                stats.skipped += 1;
                stats.skipped_m += u64::from(plan.m.is_some());
                stats.skipped_b += u64::from(plan.variant_b);
                stats.skipped_loop += u64::from(!plan.loops.is_empty());
            }
            Err(why) => self.flow_skip.stats.refused[why as usize] += 1,
        }
        result
    }

    fn flow_skip_test_worker(
        &mut self,
        reference: Node,
        declared_type: TypeId,
        initial_type: TypeId,
        flow_container: Node,
        flow: FlowNodeId,
    ) -> Result<(), Refusal> {
        if self.inline_level != 0 {
            return Err(Refusal::Inline);
        }
        let file = flow.file_index();
        if file >= self.flow_skip.files.len() {
            self.flow_skip
                .files
                .resize_with(file + 1, || FileState::Counting { walks: 0, steps: 0 });
        }
        let build_steps = self.flow_skip.build_steps;
        let state = &mut self.flow_skip.files[file];
        match state {
            FileState::Unsupported => return Err(Refusal::File),
            // A Variant B walk does not count toward the build (flowskip1
            // counted only Variant A walks, and gate projects then pay only
            // that sample, go-model-2.md 5.2). Verify mode (no threshold)
            // builds on any walk.
            FileState::Counting { .. } if initial_type != declared_type && build_steps > 0 => {
                return Err(Refusal::Counting);
            }
            FileState::Counting { walks, steps } => {
                // Only a static file (the CLI case) is indexed: its flow
                // nodes live for the process.
                let Some(flows) = static_go_file(file).and_then(|g| g.flow_nodes.get()) else {
                    *state = FileState::Unsupported;
                    return Err(Refusal::File);
                };
                *walks += 1;
                let every = sample_every(*walks);
                if *walks % every == 0 {
                    let u = flow.local_index();
                    if u < flows.len() {
                        *steps += u64::from(dry_steps(flows, file, u) * every);
                    }
                }
                if *steps < u64::from(build_steps) * flows.len() as u64 {
                    return Err(Refusal::Counting);
                }
                let index = self.flow_skip_build(file, flows.as_slice());
                self.flow_skip.files[file] = FileState::Built(index);
            }
            FileState::Built(_) => {}
        }
        let mut files = std::mem::take(&mut self.flow_skip.files);
        let FileState::Built(index) = &mut files[file] else {
            unreachable!("the file index was built above");
        };
        self.flow_skip.plan.clear();
        let result = self.flow_skip_check(
            index,
            reference,
            declared_type,
            initial_type,
            flow_container,
            flow,
        );
        if result.is_ok() {
            self.flow_skip.stats.skipped_steps += u64::from(self.flow_skip.plan.steps);
        }
        self.flow_skip.files = files;
        result
    }

    /// Builds the index of a file (once per checker and file).
    #[cold]
    #[inline(never)]
    fn flow_skip_build(&mut self, file: usize, flows: &'static [FlowNode]) -> Box<FileIndex> {
        let start = std::time::Instant::now();
        let index = FileIndex::build(file, flows, self.strict_null_checks);
        let stats = &mut self.flow_skip.stats;
        stats.built += 1;
        stats.built_nodes += flows.len() as u64;
        stats.build_ns += start.elapsed().as_nanos() as u64;
        Box::new(index)
    }

    /// Follows pi(u) through the index (go-model-2.md 2.3 and 3 with
    /// go-model-2-fix.md). First the static tests of every segment
    /// (mentions, chain sites, depth) and its end (M, a root, a loop label
    /// with its cache), then the candidates of the class, then the rule at
    /// M and the dry run of `isReachableFlowNode(M)`. Variant B takes only a
    /// path with no 2+ branch label: Go's walk is then that path too.
    fn flow_skip_check(
        &mut self,
        index: &mut FileIndex,
        reference: Node,
        declared_type: TypeId,
        initial_type: TypeId,
        flow_container: Node,
        flow: FlowNodeId,
    ) -> Result<(), Refusal> {
        let u = flow.local_index();
        if u >= index.flows.len() || index.tin[u] == NONE {
            return Err(Refusal::Unreached);
        }
        if index.steps[u] < self.flow_skip.min_steps {
            // A short walk: test it only when its root may go on (a move to
            // the containing function, or a loop entry).
            let root = index.root_of[u] as usize;
            let goes_on = match index.root_kind[root] {
                ROOT_START => index.flows[root].node.is_some(),
                ROOT_LOOP => true,
                _ => false,
            };
            if !goes_on {
                return Err(Refusal::Short);
            }
        }
        let union = self.ty(declared_type).flags.intersects(TypeFlags::UNION);
        let Some(keys) = self.flow_skip_ref_keys(reference, union) else {
            return Err(Refusal::Kind);
        };
        let m = self.flow_skip_m(index, &keys);
        if m != NONE
            && index.encloses(m as usize, u)
            && index.steps[u] - index.steps[m as usize] < self.flow_skip.min_steps_m
        {
            // A short walk to M.
            return Err(Refusal::Short);
        }
        // Variant B (an identifier whose initial type is not declared): the
        // walk must end at M, at Unreachable or at a loop hit (a Start gives
        // the initial type). getUnionOrEvolvingArrayType([declared]) is
        // declared except for unknownUnionType (checker.go:31685), and
        // isTypeSubsetOf(declared, initial) reads a declared type for a
        // non-union enum-like type (relater.go:2876).
        let variant_b = initial_type != declared_type;
        if variant_b {
            let flags = self.ty(declared_type).flags;
            if keys.kind != K_IDENT
                || m == NONE
                || declared_type == self.unknown_union_type
                || flags.intersects(TypeFlags::ENUM_LIKE) && !flags.intersects(TypeFlags::UNION)
            {
                return Err(Refusal::Region);
            }
        }
        let mut segments: SmallVec<[(u32, u32); 4]> = SmallVec::new();
        let mut loops: SmallVec<[FlowLoopKey; 2]> = SmallVec::new();
        let mut ref_key: Option<CacheHashKey> = None;
        let mut reached_m = false;
        let mut seg = u;
        let mut total_nest = 0u32;
        let mut total_steps = 0u32;
        loop {
            let p = index.tin[seg];
            if p == NONE {
                return Err(Refusal::Unreached);
            }
            // M on this segment: the walk ends there (flow.go:224).
            let at_m = m != NONE && index.encloses(m as usize, seg);
            let limit = if at_m { index.tin[m as usize] } else { NONE };
            for &key in &keys.mentions {
                let mut count = index.mentions_on_path(key, p);
                if at_m {
                    // M itself mentions R; nodes at M or above are not visited.
                    count -= index.mentions_on_path(key, limit);
                }
                if count > 0 {
                    return Err(Refusal::Mention);
                }
            }
            for &key in &keys.chains {
                if !self.flow_skip_chain_ok(index, key, p, limit) {
                    return Err(Refusal::Chain);
                }
            }
            if variant_b && index.labels[seg] != if at_m { index.labels[m as usize] } else { 0 } {
                // A 2+ branch label: Go walks all its antecedents there.
                // That region case is not skipped (go-model-2-fix.md
                // "Variant B region": a search cost about as much as Go's
                // walk on T3 Code server).
                return Err(Refusal::Region);
            }
            if at_m {
                total_nest += index.nest[seg] - index.nest[m as usize];
                if total_nest >= MAX_NEST {
                    return Err(Refusal::Depth);
                }
                total_steps += index.steps[seg] - index.steps[m as usize] + 1;
                segments.push((seg as u32, m));
                reached_m = true;
                break;
            }
            total_steps += index.steps[seg];
            total_nest += index.nest[seg];
            if total_nest >= MAX_NEST {
                return Err(Refusal::Depth);
            }
            let root = index.root_of[seg] as usize;
            segments.push((seg as u32, root as u32));
            match index.root_kind[root] {
                ROOT_UNREACHABLE => break,
                ROOT_START if variant_b => return Err(Refusal::Region),
                ROOT_START => {
                    // Go's rule for moving to the containing function
                    // (flow.go:185-191).
                    let container = index.flows[root].node;
                    let crosses = container.is_some()
                        && container != flow_container
                        && match keys.kind {
                            K_IDENT => true,
                            K_THIS => is_arrow_function(container),
                            _ => false,
                        };
                    if !crosses {
                        break;
                    }
                    let outer = container.flow_node();
                    if outer.is_nil()
                        || outer.file_index() != index.file
                        || outer.local_index() >= index.flows.len()
                    {
                        return Err(Refusal::Cross);
                    }
                    seg = outer.local_index();
                }
                ROOT_LOOP => {
                    // flow.go:1325-1400 with an inert entry path.
                    let key = match ref_key {
                        Some(key) => key,
                        None => {
                            let key = self.flow_skip_ref_key(
                                reference,
                                declared_type,
                                initial_type,
                                flow_container,
                            )?;
                            ref_key = Some(key);
                            key
                        }
                    };
                    let loop_key = FlowLoopKey {
                        flow_node: FlowNodeId::new(index.file, root),
                        ref_key: key,
                    };
                    match self.flow_skip_loop(loop_key, declared_type)? {
                        LoopRead::Hit => break,
                        LoopRead::Miss => {}
                    }
                    if loops.contains(&loop_key) {
                        return Err(Refusal::Root);
                    }
                    loops.push(loop_key);
                    // The entry nests (flow.go:1362).
                    total_nest += 1;
                    if total_nest >= MAX_NEST {
                        return Err(Refusal::Depth);
                    }
                    seg = loop_entry(index.flows, index.file, root).ok_or(Refusal::Root)? as usize;
                }
                _ => return Err(Refusal::Root),
            }
        }
        if total_steps
            < if reached_m {
                self.flow_skip.min_steps_m
            } else {
                self.flow_skip.min_steps
            }
        {
            return Err(Refusal::Short);
        }
        let class = self.flow_skip_class(reference, keys.kind, union);
        let last = segments.len() - 1;
        for (i, &(seg, _)) in segments.iter().enumerate() {
            // Candidates on the segment, through the joined ones; in the
            // segment of M, only those below M.
            let limit = if reached_m && i == last {
                index.tin[m as usize]
            } else {
                NONE
            };
            let mut s = find(&mut index.class_state(class).next, seg);
            loop {
                if limit != NONE && index.tin[s as usize] <= limit {
                    break;
                }
                let ci = index.cand_of[s as usize];
                if ci == NONE {
                    break;
                }
                self.flow_skip_candidate(index, ci, s, class, &keys)?;
                let parent = index.parent[s as usize];
                s = find(&mut index.class_state(class).next, parent);
            }
        }
        if reached_m {
            self.flow_skip_at_m(index, m, &keys, declared_type)?;
        }
        let plan = &mut self.flow_skip.plan;
        plan.loops.extend_from_slice(&loops);
        plan.path_ends.extend_from_slice(&segments);
        plan.nest = total_nest;
        plan.steps = total_steps;
        plan.variant_b = variant_b;
        Ok(())
    }

    /// The class of a reference (kind x union declared type x constant
    /// reference, go-model-fix.md "Class").
    fn flow_skip_class(&mut self, reference: Node, kind: u8, union: bool) -> Class {
        let cr = match self.flow_skip_constant_reference_cached(reference) {
            Some(false) => 0,
            Some(true) => 1,
            None => 2,
        };
        Class { kind, union, cr }
    }

    /// Settles candidate `ci` (node `s`) for `class` and checks its
    /// extras against R's keys; a candidate that has nothing to check per
    /// walk is joined to its parent.
    fn flow_skip_candidate(
        &mut self,
        index: &mut FileIndex,
        ci: u32,
        s: u32,
        class: Class,
        keys: &RefKeys,
    ) -> Result<(), Refusal> {
        let st = index.class_state(class).state[ci as usize];
        match st {
            S_BLOCKED => Err(Refusal::Blocked),
            S_EXTRAS => {
                let state = index.class_state(class);
                let extras = &state.extras[&ci];
                if self.flow_skip_extras_ok(extras, keys) {
                    Ok(())
                } else {
                    Err(Refusal::Extras)
                }
            }
            S_JOINED => Ok(()),
            _ => match self.flow_skip_settle(index, ci, class) {
                Settle::Joined => {
                    let parent = index.parent[s as usize];
                    let state = index.class_state(class);
                    state.state[ci as usize] = S_JOINED;
                    state.next[s as usize] = parent;
                    Ok(())
                }
                Settle::Extras(extras) => {
                    let ok = self.flow_skip_extras_ok(&extras, keys);
                    let state = index.class_state(class);
                    state.state[ci as usize] = S_EXTRAS;
                    state.extras.insert(ci, extras);
                    if ok { Ok(()) } else { Err(Refusal::Extras) }
                }
                Settle::Unsettled => Err(Refusal::Unsettled),
                Settle::Blocked => {
                    index.class_state(class).state[ci as usize] = S_BLOCKED;
                    Err(Refusal::Blocked)
                }
            },
        }
    }

    /// Go `getFlowReferenceKey` (flow.go:1657) for a reference of the
    /// index's kinds: the root symbol is cached (P2) and the names are
    /// literals, so the call reads only. The initial type is coalesced
    /// (CE3).
    fn flow_skip_ref_key(
        &mut self,
        reference: Node,
        declared_type: TypeId,
        initial_type: TypeId,
        flow_container: Node,
    ) -> Result<CacheHashKey, Refusal> {
        let mut b = KeyBuilder::default();
        if !self.write_flow_cache_key(
            &mut b,
            reference,
            declared_type,
            initial_type,
            flow_container,
        ) {
            return Err(Refusal::Kind);
        }
        let key = b.hash();
        if key == *NON_DOTTED_NAME_CACHE_KEY {
            return Err(Refusal::Kind);
        }
        Ok(key)
    }

    /// Go's loop label steps before the entry walk (flow.go:1336-1351):
    /// a cached declared type ends the walk there; another cached type,
    /// or the key on `flowLoopStack` with types, refuses.
    fn flow_skip_loop(&self, key: FlowLoopKey, declared_type: TypeId) -> Result<LoopRead, Refusal> {
        if let Some(&cached) = self.flow_loop_cache.get(&key)
            && cached.is_some()
        {
            return if cached == declared_type {
                Ok(LoopRead::Hit)
            } else {
                Err(Refusal::LoopOther)
            };
        }
        if self
            .flow_loop_stack
            .iter()
            .any(|info| info.key == key && !info.types.is_empty())
        {
            return Err(Refusal::LoopStack);
        }
        Ok(LoopRead::Miss)
    }

    /// The local index of M, the one Assignment flow node of a declaration
    /// of R's root symbol in this file (go-model-2-fix.md "At M"), or NONE
    /// (then the full mention test refuses a walk that meets one).
    fn flow_skip_m(&self, index: &FileIndex, keys: &RefKeys) -> u32 {
        if keys.kind != K_IDENT && keys.kind != K_ACC_IDENT {
            return NONE;
        }
        let symbol = self
            .symbol_node_links
            .try_get(keys.root)
            .map_or(SymbolId::NIL, |links| links.resolved_symbol);
        if symbol.is_nil() {
            return NONE;
        }
        let mut found = NONE;
        for &declaration in &self.sym(symbol).declarations {
            if let Some(&m) = index.decl_flow.get(&declaration) {
                if m == NONE || found != NONE {
                    return NONE;
                }
                found = m;
            }
        }
        found
    }

    /// The walk at M (go-model-2-fix.md "At M"): the match is the one
    /// Go finds (flow.go:1613-1614, 1841-1848), the result is the declared
    /// type with no query (flow.go:228-249, 259-266), and the dry run of
    /// `isReachableFlowNode(M)` (flow.go:225, 256) completes with true. Its
    /// writes go to the plan.
    fn flow_skip_at_m(
        &mut self,
        index: &FileIndex,
        m: u32,
        keys: &RefKeys,
        declared_type: TypeId,
    ) -> Result<(), Refusal> {
        let node = index.flows[m as usize].node;
        if !self.flow_skip_m_rule(node, keys, declared_type) {
            return Err(Refusal::DeclRule);
        }
        let m_id = FlowNodeId::new(index.file, m as usize);
        let start = stats_on().then(std::time::Instant::now);
        let mut overlay = std::mem::take(&mut self.flow_skip.dry_overlay);
        overlay.clear();
        let reachable =
            self.flow_skip_reach_dry(index.flows, index.file, m_id, false, &mut overlay, 0);
        if let Some(start) = start {
            self.flow_skip.stats.dry_runs += 1;
            self.flow_skip.stats.dry_ns += start.elapsed().as_nanos() as u64;
        }
        let ok = reachable == Some(true);
        if ok {
            let plan = &mut self.flow_skip.plan;
            plan.m = m_id;
            plan.reachable.extend(overlay.iter().map(|(&f, &r)| (f, r)));
        }
        self.flow_skip.dry_overlay = overlay;
        if ok { Ok(()) } else { Err(Refusal::DeclReach) }
    }

    /// The match at M and its result, for the declared type, with reads
    /// only.
    fn flow_skip_m_rule(&self, node: Node, keys: &RefKeys, declared_type: TypeId) -> bool {
        // isMatchingReference(root, M.node): the export symbol of the
        // resolved root against getSymbolOfDeclaration (pure for a symbol
        // that is not a late-bound class member).
        let root_symbol = self
            .symbol_node_links
            .try_get(keys.root)
            .map_or(SymbolId::NIL, |links| links.resolved_symbol);
        let declaration_symbol = node.symbol();
        if root_symbol.is_nil()
            || declaration_symbol.is_nil()
            || self
                .sym(declaration_symbol)
                .flags
                .intersects(SymbolFlags::CLASS_MEMBER)
            || self.get_export_symbol_of_value_symbol_if_exported(root_symbol)
                != self.get_merged_symbol(declaration_symbol)
        {
            return false;
        }
        if keys.kind == K_ACC_IDENT {
            // containsMatchingReference: an expando on a function
            // expression nests on the antecedent (flow.go:259-264).
            if is_variable_declaration(node) && (is_in_js_file(node) || is_var_const_like(node)) {
                let initializer = node.initializer();
                if initializer.is_some() && is_function_expression_or_arrow_function(initializer) {
                    return false;
                }
            }
            return true;
        }
        // A declaration is never a compound assignment target, and declared
        // is not auto (flow.go:228-243).
        if !self.ty(declared_type).flags.intersects(TypeFlags::UNION) {
            return true;
        }
        // getAssignmentReducedType(declared, getInitialOrAssignedType)
        // (flow.go:246, 276, 2244-2270, 2399-2413) with no query: the
        // initializer's cached type, which getNarrowableTypeForReference
        // keeps (no instantiable part, checker.go:31975-32008), and a result
        // that is declared with no new cache entry.
        if !is_variable_declaration(node) {
            return false;
        }
        let initializer = node.initializer();
        if initializer.is_nil() {
            return false;
        }
        let initial = self
            .type_node_links
            .try_get(initializer)
            .map_or(TypeId::NIL, |links| links.resolved_type);
        if initial.is_nil() || self.flow_skip_has_instantiable(initial) {
            return false;
        }
        if initial == declared_type {
            return true;
        }
        if self.ty(initial).flags.intersects(TypeFlags::NEVER) {
            return false;
        }
        let key = AssignmentReducedKey {
            id1: self.ty(declared_type).id,
            id2: self.ty(initial).id,
        };
        self.assignment_reduced_types.get(&key) == Some(&declared_type)
    }

    /// Whether `t` or a union or intersection member at any depth is
    /// instantiable (getNarrowableTypeForReference queries a constraint
    /// only there; a NoInfer type is a substitution type).
    fn flow_skip_has_instantiable(&self, t: TypeId) -> bool {
        let ty = self.ty(t);
        if ty.flags.intersects(TypeFlags::INSTANTIABLE) {
            return true;
        }
        if ty
            .flags
            .intersects(TypeFlags::UNION | TypeFlags::INTERSECTION)
        {
            return ty
                .types()
                .iter()
                .any(|&member| self.flow_skip_has_instantiable(member));
        }
        false
    }

    /// Dry run of Go `isReachableFlowNodeWorker` (flow.go:2522-2587) on the
    /// current caches: the same reads in the same order, with the
    /// `flowNodeReachable` writes kept in `overlay`. `None` when Go's call
    /// would make or compute something (an effects signature, predicate,
    /// return type or exhaustive state that is not cached), meets a reduce
    /// label (it clears `lastFlowNode`), or nests too deep.
    fn flow_skip_reach_dry(
        &self,
        flows: &[FlowNode],
        file: usize,
        flow: FlowNodeId,
        no_cache_check: bool,
        overlay: &mut FxHashMap<FlowNodeId, bool>,
        depth: u32,
    ) -> Option<bool> {
        if depth > MAX_DRY_DEPTH {
            return None;
        }
        let mut flow = flow;
        let mut no_cache_check = no_cache_check;
        loop {
            if flow == self.last_flow_node {
                return Some(self.last_flow_node_reachable);
            }
            // The walk stays in the file of M (antecedents of a static file).
            if flow.is_nil() || flow.file_index() != file {
                return None;
            }
            let flow_data = flows.get(flow.local_index())?;
            let flags = flow_data.flags;
            if flags.intersects(FlowFlags::SHARED) {
                if !no_cache_check {
                    if let Some(&reachable) = overlay.get(&flow) {
                        return Some(reachable);
                    }
                    if let Some(&reachable) = self.flow_node_reachable.get(&flow) {
                        return Some(reachable);
                    }
                    let reachable =
                        self.flow_skip_reach_dry(flows, file, flow, true, overlay, depth + 1)?;
                    overlay.insert(flow, reachable);
                    return Some(reachable);
                }
                no_cache_check = false;
            }
            if flags.intersects(
                FlowFlags::ASSIGNMENT | FlowFlags::CONDITION | FlowFlags::ARRAY_MUTATION,
            ) {
                flow = flow_data.antecedent;
            } else if flags.intersects(FlowFlags::CALL) {
                let signature = self
                    .signature_links
                    .try_get(flow_data.node)?
                    .effects_signature;
                if signature.is_nil() {
                    return None;
                }
                if signature != self.unknown_signature {
                    let predicate = self.sig(signature).resolved_type_predicate;
                    if predicate.is_nil() {
                        return None;
                    }
                    if predicate != self.no_type_predicate
                        && self.pred(predicate).kind == TypePredicateKind::ASSERTS_IDENTIFIER
                        && self.pred(predicate).t.is_nil()
                    {
                        let arguments = flow_data.node.arguments();
                        let parameter_index = self.pred(predicate).parameter_index;
                        if parameter_index >= 0
                            && (parameter_index as usize) < arguments.len()
                            && is_false_expression_pure(arguments.get(parameter_index as usize))
                        {
                            return Some(false);
                        }
                    }
                    let return_type = self.sig(signature).resolved_return_type;
                    if return_type.is_nil() {
                        return None;
                    }
                    if self.ty(return_type).flags.intersects(TypeFlags::NEVER) {
                        return Some(false);
                    }
                }
                flow = flow_data.antecedent;
            } else if flags.intersects(FlowFlags::BRANCH_LABEL) {
                // No reduce labels here: the run aborts at one.
                for &antecedent in &flow_data.antecedents {
                    if self.flow_skip_reach_dry(
                        flows,
                        file,
                        antecedent,
                        false,
                        overlay,
                        depth + 1,
                    )? {
                        return Some(true);
                    }
                }
                return Some(false);
            } else if flags.intersects(FlowFlags::LOOP_LABEL) {
                if flow_data.antecedents.is_empty() {
                    return Some(false);
                }
                flow = flow_data.antecedents[0];
            } else if flags.intersects(FlowFlags::SWITCH_CLAUSE) {
                let data = flow_data.as_flow_switch_clause_data();
                if data.clause_start == data.clause_end {
                    let state = self
                        .switch_statement_links
                        .try_get(data.switch_statement)
                        .map_or(ExhaustiveState::UNKNOWN, |links| links.exhaustive_state);
                    if state == ExhaustiveState::TRUE {
                        return Some(false);
                    }
                    if state != ExhaustiveState::FALSE {
                        return None;
                    }
                }
                flow = flow_data.antecedent;
            } else if flags.intersects(FlowFlags::REDUCE_LABEL) {
                return None;
            } else {
                return Some(!flags.intersects(FlowFlags::UNREACHABLE));
            }
        }
    }

    /// Whether every chain site of names key `key` that encloses the node
    /// with entry number `p`, strictly below the node with entry number
    /// `limit` (NONE: no limit), has a resolved root (go-model-fix.md
    /// item 7, go-model-2-fix.md correction 14).
    fn flow_skip_chain_ok(&self, index: &mut FileIndex, key: u64, p: u32, limit: u32) -> bool {
        let Some(&(a, b)) = index.chain_range.get(&key) else {
            return true;
        };
        let (a, b) = (a as usize, b as usize);
        let last = a + index.chain_sites[a..b].partition_point(|site| site.1 <= p);
        if last == a {
            return true;
        }
        // The deepest site that encloses p is `last - 1` or one of the sites
        // that enclose it.
        let mut x = (last - 1) as u32;
        while index.chain_sites[x as usize].2 <= p {
            x = index.chain_up[x as usize];
            if x == NONE {
                return true;
            }
        }
        let none = index.chain_sites.len() as u32;
        loop {
            x = find(&mut index.chain_skip, x);
            if x == none {
                return true;
            }
            let (_, site_tin, _, root) = index.chain_sites[x as usize];
            if limit != NONE && site_tin <= limit {
                return true;
            }
            if !self.flow_skip_resolved(root) {
                return false;
            }
            let up = index.chain_up[x as usize];
            index.chain_skip[x as usize] = if up == NONE { none } else { up };
        }
    }

    /// `flow_skip_constant_reference` with the answer for an identifier
    /// (or the identifier root of an access) kept once it is final: a
    /// constant variable stays one, and the assignments of a marked
    /// function do not change.
    fn flow_skip_constant_reference_cached(&mut self, reference: Node) -> Option<bool> {
        if reference.kind() != SyntaxKind::Identifier {
            return self.flow_skip_constant_reference(reference);
        }
        let symbol = self
            .symbol_node_links
            .try_get(reference)
            .map_or(SymbolId::NIL, |links| links.resolved_symbol);
        if let Some(&known) = self.flow_skip.constant_references.get(&symbol) {
            return Some(known);
        }
        let result = self.flow_skip_constant_reference(reference);
        if let Some(known) = result {
            self.flow_skip.constant_references.insert(symbol, known);
        }
        result
    }

    fn flow_skip_extras_ok(&self, extras: &Extras, keys: &RefKeys) -> bool {
        if extras.mentions.iter().any(|k| keys.mentions.contains(k)) {
            return false;
        }
        extras
            .chains
            .iter()
            .all(|(k, root)| !keys.chains.contains(k) || self.flow_skip_resolved(*root))
    }

    /// Whether candidate `ci` is inert for every walk of `class` once its
    /// extras pass (go-model.md 4.3).
    fn flow_skip_settle(&mut self, index: &FileIndex, ci: u32, class: Class) -> Settle {
        let cand = &index.cands[ci as usize];
        let k = usize::from(class.kind);
        if cand.blocked & (1 << (k * 2)) != 0
            || class.union && cand.blocked & (1 << (k * 2 + 1)) != 0
        {
            return Settle::Blocked;
        }
        if cand.call.is_some() {
            // getTypeAtFlowCall (flow.go:288): a cached "none" effects
            // signature continues; a real one nests or reads a return type.
            let Some(links) = self.signature_links.try_get(cand.call) else {
                return Settle::Unsettled;
            };
            let signature = links.effects_signature;
            if signature.is_nil() {
                return Settle::Unsettled;
            }
            if signature != self.unknown_signature {
                return Settle::Blocked;
            }
        }
        let bits = slot(class.kind, false)
            | if class.union {
                slot(class.kind, true)
            } else {
                0
            };
        if !index.res_pool[cand.res_start as usize..cand.res_end as usize]
            .iter()
            .all(|&(x, b)| b & bits == 0 || self.flow_skip_resolved(x))
        {
            return Settle::Unsettled;
        }
        let mut extras = Extras::default();
        for alias in &index.alias_pool[cand.alias_start as usize..cand.alias_end as usize] {
            let outcome = match *alias {
                Alias::N(x, at, d) => self.flow_skip_alias_n(x, at, d, class, &mut extras),
                Alias::D(x) => {
                    if class.union {
                        self.flow_skip_alias_d(x, class, &mut extras)
                    } else {
                        Ok(())
                    }
                }
            };
            if let Err(settle) = outcome {
                return settle;
            }
        }
        if extras.mentions.is_empty() && extras.chains.is_empty() {
            Settle::Joined
        } else {
            Settle::Extras(extras)
        }
    }

    /// An N-position identifier (flow.go:386-396): an inline alias when it
    /// is a const variable with no type and an initializer and R is a
    /// constant reference; else T(x), whose only new part is D(x).
    fn flow_skip_alias_n(
        &mut self,
        x: Node,
        at: bool,
        d: u8,
        class: Class,
        extras: &mut Extras,
    ) -> Result<(), Settle> {
        let symbol = self
            .symbol_node_links
            .try_get(x)
            .map_or(SymbolId::NIL, |links| links.resolved_symbol);
        if symbol.is_nil() {
            return Err(Settle::Unsettled);
        }
        if self.is_constant_variable(symbol) {
            let declaration = self.sym(symbol).value_declaration;
            if declaration.is_some()
                && is_variable_declaration(declaration)
                && declaration.type_().is_nil()
                && declaration.initializer().is_some()
            {
                match class.cr {
                    2 => return Err(Settle::Blocked),
                    1 => {
                        let mut scan = Scan {
                            strict: self.strict_null_checks,
                            out: Vec::new(),
                        };
                        scan.n(declaration.initializer(), at, d + 1);
                        return self.flow_skip_extras_of(&scan.out, class, extras);
                    }
                    _ => {}
                }
            }
        }
        if class.union {
            return self.flow_skip_alias_d(x, class, extras);
        }
        Ok(())
    }

    /// A D-position identifier (flow.go:1477-1491): `const x = o.p` adds
    /// M(o); `const { p: x } = o` adds M(o).
    fn flow_skip_alias_d(
        &mut self,
        x: Node,
        class: Class,
        extras: &mut Extras,
    ) -> Result<(), Settle> {
        let symbol = self
            .symbol_node_links
            .try_get(x)
            .map_or(SymbolId::NIL, |links| links.resolved_symbol);
        if symbol.is_nil() {
            return Err(Settle::Unsettled);
        }
        if !self.is_constant_variable(symbol) {
            return Ok(());
        }
        let declaration = self.sym(symbol).value_declaration;
        let initializer = get_candidate_variable_declaration_initializer(declaration);
        let target = if initializer.is_some() && is_access_expression(initializer) {
            initializer.expression()
        } else if is_binding_element(declaration) && declaration.initializer().is_nil() {
            let initializer =
                get_candidate_variable_declaration_initializer(declaration.parent().parent());
            if initializer.is_some()
                && (is_identifier(initializer) || is_access_expression(initializer))
            {
                initializer
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        };
        self.flow_skip_extras_of(&[Ev::M(target, true)], class, extras)
    }

    /// Adds the meaning of alias `events` for `class` to `extras`: its
    /// resolves must be resolved now, a block blocks the class here.
    fn flow_skip_extras_of(
        &mut self,
        events: &[Ev],
        class: Class,
        extras: &mut Extras,
    ) -> Result<(), Settle> {
        let mut out = Out::default();
        for &ev in events {
            // A non-union class leaves out the discriminant roles.
            if class.union || !matches!(ev, Ev::M(_, true) | Ev::DIdent(_)) {
                out.event(ev);
            }
        }
        let bits = slot(class.kind, false)
            | if class.union {
                slot(class.kind, true)
            } else {
                0
            };
        if out.blocked & bits != 0 {
            return Err(Settle::Blocked);
        }
        if !out
            .resolves
            .iter()
            .all(|&(x, b)| b & bits == 0 || self.flow_skip_resolved(x))
        {
            return Err(Settle::Unsettled);
        }
        // Keys of other families never equal a key of this class.
        extras.mentions.extend_from_slice(&out.mentions);
        extras.chains.extend_from_slice(&out.chains);
        for &alias in &out.aliases {
            match alias {
                Alias::N(x, at, d) => self.flow_skip_alias_n(x, at, d, class, extras)?,
                Alias::D(x) => self.flow_skip_alias_d(x, class, extras)?,
            }
        }
        Ok(())
    }

    // ──────────────────────────────────────────────────────────────────
    // Verify mode
    // ──────────────────────────────────────────────────────────────────

    fn flow_skip_snapshot(&self, resolve_names: u64) -> Snapshot {
        Snapshot {
            types: self.type_count,
            symbols: self.symbol_count,
            signatures: self.signature_count,
            diagnostics: self.diagnostics.count,
            suggestions: self.suggestion_diagnostics.count,
            union_of_unions: self.union_of_union_types.len(),
            assignment_reduced: self.assignment_reduced_types.len(),
            flow_loop_cache: self.flow_loop_cache.len(),
            last_flow_node: self.last_flow_node,
            last_flow_node_reachable: self.last_flow_node_reachable,
            flow_node_reachable: self.flow_node_reachable.len(),
            flow_analysis_disabled: self.flow_analysis_disabled,
            flow_invocation_count: self.flow_invocation_count,
            effects: self.flow_skip.effects,
            reach_off_target: self.flow_skip.reach_off_target,
            resolve_names,
        }
    }

    /// Records one loop turn of `get_type_at_flow_node` (verify mode).
    #[cold]
    pub(crate) fn flow_skip_record(&mut self, flow: FlowNodeId, depth: i32) {
        if let Some(recording) = self.flow_skip.recording.as_deref_mut() {
            recording.visited.push(flow);
            recording.max_depth = recording.max_depth.max(depth);
        }
    }

    /// The nodes that a planned Variant A walk visits, in order: each
    /// segment from its start through its parents to its end (verify mode).
    fn flow_skip_plan_path(&self, file: usize, plan: &Plan) -> Vec<FlowNodeId> {
        let Some(FileState::Built(index)) = self.flow_skip.files.get(file) else {
            return Vec::new();
        };
        let mut path = Vec::new();
        for &(start, end) in &plan.path_ends {
            let mut x = start;
            loop {
                path.push(FlowNodeId::new(file, x as usize));
                if x == end || index.parent[x as usize] == NONE {
                    break;
                }
                x = index.parent[x as usize];
            }
        }
        path
    }

    /// Verify mode: runs Go's walk of a reference that the test skips and
    /// panics when the walk does anything that the skip leaves out or
    /// writes anything other than the plan.
    pub(crate) fn flow_skip_verify(
        &mut self,
        reference: Node,
        declared_type: TypeId,
        initial_type: TypeId,
        flow_container: Node,
        flow: FlowNodeId,
    ) -> TypeId {
        let plan = std::mem::take(&mut self.flow_skip.plan);
        let file = flow.file_index();
        let expected_nodes = self.flow_skip_plan_path(file, &plan);
        let calls = Rc::new(Cell::new(0u64));
        let resolve_name = self.resolve_name.clone();
        {
            let calls = calls.clone();
            let inner = resolve_name.clone();
            self.resolve_name = Rc::new(
                move |c, location, name, meaning, not_found, is_use, exclude| {
                    calls.set(calls.get() + 1);
                    inner(c, location, name, meaning, not_found, is_use, exclude)
                },
            );
        }
        let before = self.flow_skip_snapshot(0);
        let absent_before = plan
            .reachable
            .iter()
            .all(|(f, _)| !self.flow_node_reachable.contains_key(f))
            && plan
                .loops
                .iter()
                .all(|k| self.flow_loop_cache.get(k).is_none_or(|t| t.is_nil()));
        let outer_target = std::mem::replace(&mut self.flow_skip.reach_target, plan.m);
        // A walk that is not inert can start other walks, which a nested
        // verify records on its own.
        let outer = self.flow_skip.recording.replace(Box::default());
        let start = stats_on().then(std::time::Instant::now);
        let result = self.flow_walk(reference, declared_type, initial_type, flow_container, flow);
        if let Some(start) = start {
            self.flow_skip.stats.skip_walk_ns[plan.category()] += start.elapsed().as_nanos() as u64;
        }
        let recording = std::mem::replace(&mut self.flow_skip.recording, outer)
            .expect("the recording of this walk");
        self.flow_skip.reach_target = outer_target;
        let after = self.flow_skip_snapshot(calls.get());
        self.resolve_name = resolve_name;
        // What Go's walk must have written: the plan.
        let mut expected = before;
        expected.flow_loop_cache += plan.loops.len();
        expected.flow_node_reachable += plan.reachable.len();
        if plan.m.is_some() {
            expected.last_flow_node = plan.m;
            expected.last_flow_node_reachable = true;
        }
        let writes_match = plan
            .reachable
            .iter()
            .all(|(f, r)| self.flow_node_reachable.get(f) == Some(r))
            && plan
                .loops
                .iter()
                .all(|k| self.flow_loop_cache.get(k) == Some(&declared_type));
        let visited = &recording.visited;
        let problem = if result != declared_type {
            let result_text = self.type_to_string(result);
            let declared_text = self.type_to_string(declared_type);
            Some(format!(
                "the walk returned type {:?} ({result_text}), not the declared type {:?} ({declared_text})",
                self.ty(result).id,
                self.ty(declared_type).id,
            ))
        } else if after != expected || !absent_before || !writes_match {
            Some(format!(
                "the walk changed state other than the plan: before {before:?} after {after:?} expected {expected:?} (absent before {absent_before}, writes match {writes_match}, plan m {:?}, {} reachable, {} loops)",
                plan.m,
                plan.reachable.len(),
                plan.loops.len()
            ))
        } else if *visited != expected_nodes {
            let first = visited
                .iter()
                .zip(expected_nodes.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(visited.len().min(expected_nodes.len()));
            Some(format!(
                "visited {} nodes, the plan has {} (variant b {}); first difference at {first}: walk {:?} plan {:?}",
                visited.len(),
                expected_nodes.len(),
                plan.variant_b,
                visited.get(first).map(|f| f.get_flow().flags),
                expected_nodes.get(first).map(|f| f.get_flow().flags)
            ))
        } else if recording.max_depth != plan.nest as i32 + 1 {
            Some(format!(
                "maximum depth {} but the plan nests {} (variant b {})",
                recording.max_depth, plan.nest, plan.variant_b
            ))
        } else {
            None
        };
        self.flow_skip.plan = plan;
        if let Some(problem) = problem {
            let source_file = get_source_file_of_node(reference);
            panic!(
                "flowskip verify: {} pos {} reference `{}` ({:?}): {problem}",
                source_file_file_name(source_file),
                reference.pos(),
                get_text_of_node(reference),
                reference.kind()
            );
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks `a.ts` (`source`) in a new project with `options` (JSON
    /// members of `compilerOptions`), with the skip in verify mode and every
    /// walk tested. Verify mode panics when a skipped walk differs from
    /// Go's walk. Returns the diagnostic codes and, per tested reference,
    /// its text, its position and whether it was skipped.
    fn check_with_skip(
        label: &str,
        source: &str,
        options: &str,
    ) -> (Vec<i32>, Vec<(String, i32, bool)>) {
        let dir =
            std::env::temp_dir().join(format!("ts_goport_flowskip_{label}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.ts"), source).unwrap();
        std::fs::write(
            dir.join("tsconfig.json"),
            format!(
                r#"{{ "compilerOptions": {{ {options}, "target": "es2020", "types": [] }}, "files": ["a.ts"] }}"#
            ),
        )
        .unwrap();
        let config = dir.join("tsconfig.json");
        let program = crate::program::try_load_version(&config.to_string_lossy(), |_| {})
            .unwrap_or_else(|e| panic!("cannot load {}: {e}", config.display()));
        let _ = std::fs::remove_dir_all(&dir);
        let scope = crate::core::enter_program(Some(program));
        let file = program
            .source_files()
            .find(|file| file.info.file_name.ends_with("/a.ts"))
            .expect("a.ts is not in the program")
            .root;
        let result = crate::program::with_type_checker_for_file(file, move |checker| {
            checker.flow_skip.mode = FlowSkipMode::Verify;
            checker.flow_skip.build_steps = 0;
            checker.flow_skip.min_steps = 0;
            checker.flow_skip.min_steps_m = 0;
            checker.flow_skip.trace = Some(Vec::new());
            let ctx = crate::gostd::context::background();
            let codes: Vec<i32> = checker
                .get_diagnostics(&ctx, file, false)
                .iter()
                .map(|d| d.code())
                .collect();
            let trace = checker
                .flow_skip
                .trace
                .take()
                .unwrap()
                .into_iter()
                .map(|(node, skipped)| (get_text_of_node(node), node.end(), skipped))
                .collect();
            (codes, trace)
        });
        drop(scope);
        crate::program::release_program(program);
        result
    }

    /// Whether the test skipped the reference whose text is `text` and
    /// that ends at the end of the `n`th (from 0) match of `marker`.
    fn skipped_at(
        trace: &[(String, i32, bool)],
        source: &str,
        marker: &str,
        text: &str,
    ) -> Vec<bool> {
        let end = (source.find(marker).expect("marker") + marker.len()) as i32;
        trace
            .iter()
            .filter(|(t, e, _)| t == text && *e == end)
            .map(|(_, _, skipped)| *skipped)
            .collect()
    }

    /// Plain walks through conditions on other names are skipped, and
    /// verify mode finds Go's walk equal.
    #[test]
    fn plain_walks_are_skipped() {
        let source = "declare const c: boolean;\n\
            export function f(input: { a: number }, y: number) {\n\
            if (c) {} if (c) {} if (c) {} if (c) {} if (c) {}\n\
            const z = input.a + y;\n\
            return input;\n\
            }\n";
        let (codes, trace) = check_with_skip("plain", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(
            skipped_at(&trace, source, "return input", "input"),
            vec![true]
        );
        assert_eq!(skipped_at(&trace, source, "input.a", "input.a"), vec![true]);
    }

    /// Check counterexample 1a: an inline alias `p || q` with assumeTrue
    /// calls getUnionType (flow.go:549), which makes an origin union for
    /// `S | undefined`. The walk of `x` must not be skipped.
    #[test]
    fn union_making_alias_is_not_skipped() {
        let source = "type S = \"a\" | \"b\";\n\
            declare const p: boolean, q: boolean;\n\
            export function f(y: number, x: S | undefined) {\n\
            const ok = p || q;\n\
            if (ok) {}\n\
            y;\n\
            return x;\n\
            }\n";
        let (codes, trace) = check_with_skip("union_alias", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(skipped_at(&trace, source, "return x", "x"), vec![false]);
    }

    /// Check counterexample 1b: with an unresolved annotation, Go's walk
    /// returns errorType through getUnionType([E, E]); a skip would give
    /// `Missing` in the declaration.
    #[test]
    fn union_making_alias_on_error_type_is_not_skipped() {
        let source = "declare const p: boolean, q: boolean;\n\
            export function f(x: Missing) {\n\
            const ok = p || q;\n\
            if (!ok) throw 0;\n\
            x;\n\
            return x;\n\
            }\n";
        let (codes, trace) = check_with_skip("error_alias", source, r#""strict": true"#);
        assert_eq!(codes, vec![2304]);
        assert_eq!(skipped_at(&trace, source, "return x", "x"), vec![false]);
    }

    /// Check counterexample 2: `"b" in a` narrows `a.b` when its type has a
    /// missing type (flow.go:523, exactOptionalPropertyTypes).
    #[test]
    fn in_operand_prefix_is_not_skipped() {
        let source = "declare const a: { b?: string };\n\
            declare const z: boolean;\n\
            a.b;\n\
            if (z) {} if (z) {} if (z) {} if (z) {}\n\
            if (z) {} if (z) {} if (z) {} if (z) {}\n\
            if (\"b\" in a) {\n\
            const s: string = a.b;\n\
            }\n";
        let (codes, trace) = check_with_skip(
            "in_operand",
            source,
            r#""strict": true, "exactOptionalPropertyTypes": true"#,
        );
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(
            skipped_at(&trace, source, "string = a.b", "a.b"),
            vec![false]
        );
    }

    /// Check gap 3: a key site in an inline alias initializer resolves the
    /// key on every walk (flow.go:1753).
    #[test]
    fn key_site_in_alias_is_not_skipped() {
        let source = "declare const o: { [k: string]: { p: number } | undefined };\n\
            declare const K: string;\n\
            export function f(x: number) {\n\
            const ok = o[K]?.p;\n\
            if (ok) {}\n\
            x;\n\
            return x;\n\
            }\n";
        let (codes, trace) = check_with_skip("key_alias", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(skipped_at(&trace, source, "return x", "x"), vec![false]);
    }

    /// `switch (true)` narrows by its case expressions (flow.go:1187).
    #[test]
    fn switch_true_is_not_skipped() {
        let source = "export function f(x: string | number, y: number) {\n\
            switch (true) { case typeof x === \"string\": break; }\n\
            y;\n\
            return x;\n\
            }\n";
        let (codes, trace) = check_with_skip("switch_true", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(skipped_at(&trace, source, "return x", "x"), vec![false]);
    }

    /// flowskip2 Variant A: a use in an arrow function of an outer const
    /// walks into the outer function and ends at the declaration M
    /// (flow.go:224); the skip dry-runs `isReachableFlowNode(M)`.
    #[test]
    fn declaration_on_path_is_skipped() {
        let source = "declare const c: boolean;\n\
            export function f() {\n\
            const x = 1 + 2;\n\
            if (c) {} if (c) {} if (c) {} if (c) {} if (c) {}\n\
            const g = (): void => { if (c) {} if (c) {} x; };\n\
            return g;\n\
            }\n";
        let (codes, trace) = check_with_skip("decl_a", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(skipped_at(&trace, source, "if (c) {} x", "x"), vec![true]);
    }

    /// flowskip2 Variant A at M with a union declared type: the
    /// initializer's cached type is the declared type (flow.go:246).
    #[test]
    fn union_declaration_is_skipped() {
        let source = "declare const c: boolean;\n\
            declare function g(): string | number;\n\
            export function f() {\n\
            const w: string | number = g();\n\
            if (c) {} if (c) {} if (c) {}\n\
            const h = (): void => { if (c) {} w; };\n\
            return h;\n\
            }\n";
        let (codes, trace) = check_with_skip("decl_union", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(skipped_at(&trace, source, "if (c) {} w", "w"), vec![true]);
    }

    /// flowskip2 Variant B: a strict local whose initial type is
    /// `number | undefined`; the path to M has no 2+ branch label, so Go's
    /// walk is that path and returns the declared type.
    #[test]
    fn variant_b_path_is_skipped() {
        let source = "declare function g(): number;\n\
            export function f() {\n\
            let x = g();\n\
            let a = g(); let b = g(); let c = g(); let d = g();\n\
            x;\n\
            return x + a + b + c + d;\n\
            }\n";
        let (codes, trace) = check_with_skip("path_b", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(skipped_at(&trace, source, "\nx", "x"), vec![true]);
        assert_eq!(skipped_at(&trace, source, "return x", "x"), vec![true]);
    }

    /// Variant B with a 2+ branch label on the path: Go walks every
    /// antecedent there (flow.go:1269 needs declared == initial). Not
    /// skipped.
    #[test]
    fn variant_b_branch_is_not_skipped() {
        let source = "declare const c: boolean;\n\
            declare function g(): number;\n\
            export function f() {\n\
            let x = g();\n\
            if (c) {} else {}\n\
            x;\n\
            }\n";
        let (codes, trace) = check_with_skip("branch_b", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(skipped_at(&trace, source, "\nx", "x"), vec![false]);
    }

    /// flowskip2 loops: a parameter used in a loop body walks to the loop
    /// label, misses the cache, goes on at the entry, and the skip writes
    /// the loop cache entry as Go does (flow.go:1400); the next use hits it.
    #[test]
    fn loop_entry_is_skipped() {
        let source = "declare const c: boolean;\n\
            export function f(p: number) {\n\
            for (let i = 0; i < 3; i++) {\n\
            if (c) {} if (c) {}\n\
            p;\n\
            (p);\n\
            }\n\
            }\n";
        let (codes, trace) = check_with_skip("loop_a", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(
            skipped_at(&trace, source, "\np", "p"),
            vec![true],
            "trace {trace:?}"
        );
        assert_eq!(
            skipped_at(&trace, source, "\n(p", "p"),
            vec![true],
            "trace {trace:?}"
        );
    }

    /// Check CE3 of go-model-2.md: `this` walks with a nil initial type;
    /// the loop key is built with Go's coalesced initial type (flow.go:94),
    /// so verify mode finds Go's loop cache write equal to the plan.
    #[test]
    fn this_loop_key_uses_coalesced_initial() {
        let source = "declare const c: boolean;\n\
            export class A {\n\
            m() {\n\
            for (let i = 0; i < 2; i++) {\n\
            if (c) {} if (c) {}\n\
            this;\n\
            }\n\
            }\n\
            }\n";
        let (codes, trace) = check_with_skip("this_loop", source, r#""strict": true"#);
        assert_eq!(codes, Vec::<i32>::new());
        assert_eq!(
            skipped_at(&trace, source, "\nthis", "this"),
            vec![true],
            "trace {trace:?}"
        );
    }

    /// Check CE1 of go-model-2.md: the second use's Variant B region nests
    /// past 2000 (TS2563 in Go). The skip must not fire for either use
    /// (a region with 2+ branch labels is not skipped at all).
    #[test]
    fn deep_region_is_not_skipped() {
        let mut source = String::from(
            "declare function g(): number;\n\
             export function f() {\n\
             const x = g();\n",
        );
        // Each block's `k` walks one step to its own declaration, so only
        // the two uses of `x` walk the 1,100 conditions (2 nest steps each
        // in Variant B: the label, then the condition).
        for _ in 0..700 {
            source.push_str("{ const k = g(); if (k) {} }\n");
        }
        source.push_str("x;\n");
        for _ in 0..400 {
            source.push_str("{ const k = g(); if (k) {} }\n");
        }
        source.push_str("x; // u2\n}\n");
        let (codes, trace) = check_with_skip("deep_b", &source, r#""strict": true"#);
        assert!(codes.contains(&2563), "codes {codes:?}");
        let u2_end = (source.find("x; // u2").unwrap() + 1) as i32;
        let u2: Vec<bool> = trace
            .iter()
            .filter(|(t, e, _)| t == "x" && *e == u2_end)
            .map(|r| r.2)
            .collect();
        let xs: Vec<_> = trace.iter().filter(|r| r.0 == "x").collect();
        assert_eq!(
            u2,
            vec![false],
            "codes {codes:?} x walks {xs:?} u2 end {u2_end}"
        );
        let u1_end = (source.find("x;\n").unwrap() + 1) as i32;
        let u1: Vec<bool> = trace
            .iter()
            .filter(|(t, e, _)| t == "x" && *e == u1_end)
            .map(|r| r.2)
            .collect();
        assert_eq!(u1, vec![false]);
    }
}
