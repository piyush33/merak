//! Control flow and control dependence for one function body.
//!
//! Each statement becomes a node of a control-flow graph; `if` statements are
//! the only branches with a condition (loops and `try` branch too, without one).
//! A statement is *control dependent* on a branch edge when taking that edge
//! decides whether the statement runs (Ferrante, Ottenstein, Warren 1987): it
//! post-dominates the edge's target but not the branch itself. The condition a
//! statement runs under is then the disjunction, over the edges it depends on,
//! of that edge's condition and the branch's own condition. Early exits,
//! nested checks, `else` arms and `continue` all fall out of the graph instead
//! of being special cases.
//!
//! `Jump` (break/continue) leaves the innermost enclosing loop body or block,
//! which is what a `switch` lowered to a block of `if`s needs from `break`.
//!
//! Conditions come in two kinds. A branch where exactly one side always ends the
//! function (it returns or throws, or nothing is left to run) is a check the rest of
//! the function *requires*, wherever it sits: `if err != nil { return err }`. Every
//! other branch is a real alternative, and the statements on each side run *when*
//! its condition holds.

use merak_ir::{Expr, Stmt};
use std::collections::{HashMap, HashSet};

/// Which way out of a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arm {
    /// The `then` side of an `if`: its test holds.
    Then,
    /// The `else` side (or the fall-through past an `if` without one): its test fails.
    Else,
    /// An edge with no condition of its own (loop entry or exit, `try`).
    Plain,
}

const ENTRY: usize = 0;
const EXIT: usize = 1;

pub struct ControlDeps {
    node_of: HashMap<*const Stmt, usize>,
    /// Per node: the branch edges it is control dependent on.
    deps: Vec<Vec<(usize, Arm)>>,
    /// Branch edges whose other side ends the function: required checks.
    guard_edges: HashSet<(usize, Arm)>,
    /// Nodes inside a loop body.
    in_loop: HashSet<usize>,
    /// `if` nodes after which the function always ends (`if (c) { return a }; return b`).
    rest_ends: HashSet<usize>,
    /// Reaching definitions: per node, the assignments of each local that can reach its
    /// entry, by the node that makes them.
    reaching: std::cell::OnceCell<Reaching>,
    succ: Vec<Vec<(usize, Arm)>>,
    defines: Vec<Vec<String>>,
}

/// Reaching definitions as bitsets over numbered assignments.
struct Reaching {
    /// Assignment number → (node, local).
    defs: Vec<(usize, String)>,
    /// Local → the assignments of it.
    of_var: HashMap<String, Vec<usize>>,
    /// Per node: the assignments reaching its entry.
    ins: Vec<Vec<u64>>,
}

impl ControlDeps {
    pub fn build(body: &[Stmt]) -> ControlDeps {
        let mut b = Builder {
            succ: vec![vec![], vec![]],
            node_of: HashMap::new(),
            guard_edges: HashSet::new(),
            in_loop: HashSet::new(),
            rest_ends: HashSet::new(),
            defines: vec![vec![], vec![]],
            loops: 0,
            catch: vec![],
        };
        let first = b.list(body, EXIT, EXIT, false);
        // Augmented entry: the whole body is control dependent on entering the function.
        b.succ[ENTRY] = vec![(first, Arm::Plain), (EXIT, Arm::Plain)];
        let deps = control_deps(&b.succ);
        ControlDeps {
            node_of: b.node_of,
            deps,
            guard_edges: b.guard_edges,
            in_loop: b.in_loop,
            rest_ends: b.rest_ends,
            reaching: std::cell::OnceCell::new(),
            succ: b.succ,
            defines: b.defines,
        }
    }

    /// The nodes whose assignment of `var` can reach node `n` (empty: none in this body,
    /// so a parameter or an outer name).
    pub fn reaching(&self, n: usize, var: &str) -> Vec<usize> {
        let r = self.reaching.get_or_init(|| reaching_defs(&self.succ, &self.defines));
        let (Some(ins), Some(ds)) = (r.ins.get(n), r.of_var.get(var)) else { return vec![] };
        ds.iter().filter(|&&i| ins[i / 64] >> (i % 64) & 1 == 1).map(|&i| r.defs[i].0).collect()
    }

    pub fn node(&self, s: &Stmt) -> Option<usize> {
        self.node_of.get(&(s as *const Stmt)).copied()
    }

    pub fn deps(&self, n: usize) -> &[(usize, Arm)] {
        self.deps.get(n).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether everything after this `if` ends the function, so that an empty side of it
    /// ends the function too.
    pub fn rest_ends(&self, n: usize) -> bool {
        self.rest_ends.contains(&n)
    }

    /// Whether the node sits inside a loop body.
    pub fn in_loop(&self, n: usize) -> bool {
        self.in_loop.contains(&n)
    }

    /// Whether taking the edge passes a required check (the branch's other side ends the function).
    pub fn is_guard(&self, branch: usize, arm: Arm) -> bool {
        self.guard_edges.contains(&(branch, arm))
    }
}

struct Builder {
    succ: Vec<Vec<(usize, Arm)>>,
    node_of: HashMap<*const Stmt, usize>,
    guard_edges: HashSet<(usize, Arm)>,
    in_loop: HashSet<usize>,
    rest_ends: HashSet<usize>,
    /// Per node: the locals it assigns.
    defines: Vec<Vec<String>>,
    loops: u32,
    /// Where a `throw` goes: the innermost enclosing handler.
    catch: Vec<usize>,
}

/// Whether a statement list always ends the function: it ends in a `return` or
/// `throw`, or in an `if`/`else` (or block) whose every side does.
fn always_exits(stmts: &[Stmt]) -> bool {
    match stmts.last() {
        Some(Stmt::Return(..) | Stmt::Throw(..)) => true,
        Some(Stmt::If { then, otherwise, .. }) => !otherwise.is_empty() && always_exits(then) && always_exits(otherwise),
        Some(Stmt::Block(b)) => always_exits(b),
        _ => false,
    }
}

impl Builder {
    fn alloc(&mut self, s: &Stmt) -> usize {
        let n = self.succ.len();
        self.succ.push(vec![]);
        self.defines.push(assigned(s));
        self.node_of.insert(s as *const Stmt, n);
        if self.loops > 0 {
            self.in_loop.insert(n);
        }
        n
    }

    /// Lower a statement list that continues at `next`; `jump` is where a break/continue
    /// goes. Returns the list's entry node (`next` when it is empty).
    /// `next_ends`: whatever follows the list always ends the function.
    fn list(&mut self, stmts: &[Stmt], next: usize, jump: usize, next_ends: bool) -> usize {
        let mut at = next;
        for (i, s) in stmts.iter().enumerate().rev() {
            let rest = &stmts[i + 1..];
            let rest_ends = if rest.is_empty() { next_ends } else { always_exits(rest) };
            at = self.stmt(s, at, jump, rest_ends);
        }
        at
    }

    fn stmt(&mut self, s: &Stmt, next: usize, jump: usize, rest_ends: bool) -> usize {
        let n = self.alloc(s);
        let succ = match s {
            Stmt::Expr(..) | Stmt::Let { .. } => vec![(next, Arm::Plain)],
            Stmt::Return(..) => vec![(EXIT, Arm::Plain)],
            Stmt::Throw(..) => vec![(self.catch.last().copied().unwrap_or(EXIT), Arm::Plain)],
            Stmt::Jump(_) => vec![(jump, Arm::Plain)],
            Stmt::If { then, otherwise, .. } => {
                // A side ends the function if it always returns or throws, or if it is
                // empty and nothing follows the `if`. Exactly one such side makes the
                // branch a required check for the other side; both make it an alternative.
                let ends = |arm: &[Stmt]| if arm.is_empty() { next == EXIT } else { always_exits(arm) };
                match (ends(then), ends(otherwise)) {
                    (true, false) => {
                        self.guard_edges.insert((n, Arm::Else));
                    }
                    (false, true) => {
                        self.guard_edges.insert((n, Arm::Then));
                    }
                    _ => {}
                }
                if rest_ends {
                    self.rest_ends.insert(n);
                }
                let t = self.list(then, next, jump, rest_ends);
                let e = self.list(otherwise, next, jump, rest_ends);
                vec![(t, Arm::Then), (e, Arm::Else)]
            }
            Stmt::Loop { body, .. } => {
                // The loop node is its header: each pass enters the body or leaves.
                self.loops += 1;
                let b = self.list(body, n, n, false);
                self.loops -= 1;
                vec![(b, Arm::Plain), (next, Arm::Plain)]
            }
            Stmt::Try { body, handler, finalizer, .. } => {
                let fin = self.list(finalizer, next, jump, rest_ends);
                let h = self.list(handler, fin, jump, false);
                let has_handler = !handler.is_empty();
                if has_handler {
                    self.catch.push(h);
                }
                let b = self.list(body, fin, jump, false);
                if has_handler {
                    self.catch.pop();
                    vec![(b, Arm::Plain), (h, Arm::Plain)]
                } else {
                    vec![(b, Arm::Plain)]
                }
            }
            Stmt::Block(b) => {
                let first = self.list(b, next, next, rest_ends);
                vec![(first, Arm::Plain)]
            }
        };
        self.succ[n] = succ;
        n
    }
}

/// The locals a statement assigns itself (not in nested statements): `let x`, `x = …`,
/// a loop's binding, and assignments nested in its expressions (`if ((m = re.exec(s)))`).
fn assigned(s: &Stmt) -> Vec<String> {
    fn walk(e: &Expr, out: &mut Vec<String>) {
        match e {
            Expr::Assign { target, value, .. } => {
                if let Expr::Ident(x) = target.as_ref() {
                    out.push(x.clone());
                }
                walk(value, out);
            }
            Expr::Member { object, .. } => walk(object, out),
            Expr::Index { object, index } => {
                walk(object, out);
                walk(index, out);
            }
            Expr::Call(c) => {
                walk(&c.callee, out);
                c.args.iter().for_each(|a| walk(a, out));
            }
            Expr::New { callee, args, .. } => {
                walk(callee, out);
                args.iter().for_each(|a| walk(a, out));
            }
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            Expr::Not(x) | Expr::Await(x) | Expr::Spawn(x) => walk(x, out),
            Expr::Conditional { test, then, otherwise } => {
                walk(test, out);
                walk(then, out);
                walk(otherwise, out);
            }
            Expr::Template { exprs, .. } | Expr::Array(exprs) => exprs.iter().for_each(|x| walk(x, out)),
            Expr::Object(props) => props.iter().for_each(|(_, v)| walk(v, out)),
            _ => {}
        }
    }
    let mut out = vec![];
    match s {
        Stmt::Let { name, init, .. } => {
            out.push(name.clone());
            if let Some(i) = init {
                walk(i, &mut out);
            }
        }
        Stmt::Expr(e, _) | Stmt::Throw(e, _) | Stmt::If { test: e, .. } => walk(e, &mut out),
        Stmt::Return(Some(e), _) => walk(e, &mut out),
        Stmt::Loop { binding, iter, .. } => {
            if let Some(i) = iter {
                walk(i, &mut out);
            }
            out.extend(binding.clone());
        }
        _ => {}
    }
    out.sort();
    out.dedup();
    out
}

/// Reaching definitions (forward, may): a node's assignment of a local reaches every node
/// it can flow to without passing another assignment of the same local.
fn reaching_defs(succ: &[Vec<(usize, Arm)>], defines: &[Vec<String>]) -> Reaching {
    let n = succ.len();
    let mut defs: Vec<(usize, String)> = vec![];
    let mut of_var: HashMap<String, Vec<usize>> = HashMap::new();
    let mut gen: Vec<Vec<usize>> = vec![vec![]; n];
    for (v, names) in defines.iter().enumerate() {
        for x in names {
            of_var.entry(x.clone()).or_default().push(defs.len());
            gen[v].push(defs.len());
            defs.push((v, x.clone()));
        }
    }
    let words = defs.len().div_ceil(64).max(1);
    // Per node: the assignments it kills (every assignment of the locals it assigns).
    let kill: Vec<Vec<u64>> = (0..n)
        .map(|v| {
            let mut k = vec![0u64; words];
            for x in &defines[v] {
                for &i in &of_var[x] {
                    k[i / 64] |= 1 << (i % 64);
                }
            }
            k
        })
        .collect();
    let mut preds: Vec<Vec<usize>> = vec![vec![]; n];
    for (a, out) in succ.iter().enumerate() {
        for &(b, _) in out {
            preds[b].push(a);
        }
    }
    // Reverse postorder from the entry, for fast convergence.
    let mut order = vec![];
    let mut seen = vec![false; n];
    let mut stack = vec![(ENTRY, 0usize)];
    seen[ENTRY] = true;
    while let Some((v, i)) = stack.pop() {
        if i < succ[v].len() {
            stack.push((v, i + 1));
            let w = succ[v][i].0;
            if !seen[w] {
                seen[w] = true;
                stack.push((w, 0));
            }
        } else {
            order.push(v);
        }
    }
    order.reverse();
    let mut ins = vec![vec![0u64; words]; n];
    let mut outs = vec![vec![0u64; words]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for &v in &order {
            let mut inn = vec![0u64; words];
            for &p in &preds[v] {
                for (w, o) in inn.iter_mut().zip(&outs[p]) {
                    *w |= o;
                }
            }
            let mut out: Vec<u64> = inn.iter().zip(&kill[v]).map(|(i, k)| i & !k).collect();
            for &i in &gen[v] {
                out[i / 64] |= 1 << (i % 64);
            }
            if out != outs[v] {
                outs[v] = out;
                changed = true;
            }
            ins[v] = inn;
        }
    }
    Reaching { defs, of_var, ins }
}

/// Immediate post-dominators (Cooper, Harvey, Kennedy: "A Simple, Fast Dominance
/// Algorithm", run on the reversed graph from the exit), then control dependence.
fn control_deps(succ: &[Vec<(usize, Arm)>]) -> Vec<Vec<(usize, Arm)>> {
    let n = succ.len();
    let mut preds_rev: Vec<Vec<usize>> = vec![vec![]; n]; // reverse-graph successors = CFG predecessors
    for (a, out) in succ.iter().enumerate() {
        for &(b, _) in out {
            preds_rev[b].push(a);
        }
    }
    // Postorder of the reversed graph from EXIT.
    let mut order = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack = vec![(EXIT, 0usize)];
    seen[EXIT] = true;
    while let Some((v, i)) = stack.pop() {
        if i < preds_rev[v].len() {
            stack.push((v, i + 1));
            let w = preds_rev[v][i];
            if !seen[w] {
                seen[w] = true;
                stack.push((w, 0));
            }
        } else {
            order.push(v);
        }
    }
    let mut po = vec![usize::MAX; n];
    for (i, &v) in order.iter().enumerate() {
        po[v] = i;
    }
    const UNDEF: usize = usize::MAX;
    let mut ipdom = vec![UNDEF; n];
    ipdom[EXIT] = EXIT;
    let intersect = |ipdom: &[usize], mut a: usize, mut b: usize| {
        while a != b {
            while po[a] < po[b] {
                a = ipdom[a];
            }
            while po[b] < po[a] {
                b = ipdom[b];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &v in order.iter().rev() {
            if v == EXIT {
                continue;
            }
            // In the reversed graph, v's predecessors are its CFG successors.
            let mut new = UNDEF;
            for &(s, _) in &succ[v] {
                if ipdom[s] == UNDEF {
                    continue;
                }
                new = if new == UNDEF { s } else { intersect(&ipdom, s, new) };
            }
            if new != UNDEF && ipdom[v] != new {
                ipdom[v] = new;
                changed = true;
            }
        }
    }
    let post_dominates = |d: usize, mut v: usize| loop {
        if v == d {
            return true;
        }
        if v == EXIT || ipdom[v] == UNDEF {
            return false;
        }
        v = ipdom[v];
    };
    let mut deps: Vec<Vec<(usize, Arm)>> = vec![vec![]; n];
    for (a, out) in succ.iter().enumerate() {
        if ipdom[a] == UNDEF {
            continue;
        }
        for &(b, arm) in out {
            if post_dominates(b, a) {
                continue;
            }
            let mut r = b;
            while r != ipdom[a] && r != UNDEF {
                if !deps[r].contains(&(a, arm)) {
                    deps[r].push((a, arm));
                }
                if r == EXIT {
                    break;
                }
                r = ipdom[r];
            }
        }
    }
    deps
}

#[cfg(test)]
mod tests {
    use super::*;
    use merak_ir::{Expr, Loc};

    fn loc() -> Loc {
        Loc { file: "t".into(), line: 1 }
    }
    fn call(name: &str) -> Stmt {
        Stmt::Expr(Expr::Ident(name.into()), loc())
    }
    fn iff(test: &str, then: Vec<Stmt>, otherwise: Vec<Stmt>) -> Stmt {
        Stmt::If { test: Expr::Ident(test.into()), then, otherwise, loc: loc() }
    }
    fn ret() -> Stmt {
        Stmt::Return(None, loc())
    }

    /// The branches (by statement index path) a statement depends on.
    fn deps_of(cd: &ControlDeps, s: &Stmt) -> Vec<(usize, Arm)> {
        let mut d = cd.deps(cd.node(s).unwrap()).to_vec();
        d.sort_by_key(|(n, a)| (*n, *a as u8));
        d
    }

    #[test]
    fn early_exits_guard_what_follows() {
        // if (!o) return; if (bad) return; save()
        let body = vec![iff("noOrder", vec![ret()], vec![]), iff("bad", vec![ret()], vec![]), call("save")];
        let cd = ControlDeps::build(&body);
        let second = cd.node(&body[1]).unwrap();
        let first = cd.node(&body[0]).unwrap();
        assert_eq!(deps_of(&cd, &body[2]), vec![(second, Arm::Else)]);
        assert_eq!(deps_of(&cd, &body[1]), vec![(first, Arm::Else)]);
        assert!(cd.is_guard(first, Arm::Else) && cd.is_guard(second, Arm::Else));
    }

    #[test]
    fn a_return_in_the_else_arm_conditions_what_follows() {
        // if (c) { a() } else { return }; f()
        let body = vec![iff("c", vec![call("a")], vec![ret()]), call("f")];
        let cd = ControlDeps::build(&body);
        let c = cd.node(&body[0]).unwrap();
        assert_eq!(deps_of(&cd, &body[1]), vec![(c, Arm::Then)]);
        assert!(cd.is_guard(c, Arm::Then));
    }

    #[test]
    fn returns_on_both_sides_are_alternatives() {
        // if (c) { return a } else { return b }
        let body = vec![iff("c", vec![ret()], vec![ret()])];
        let cd = ControlDeps::build(&body);
        let c = cd.node(&body[0]).unwrap();
        assert!(!cd.is_guard(c, Arm::Then) && !cd.is_guard(c, Arm::Else));
    }

    #[test]
    fn a_trailing_if_guards_its_body() {
        // f(); if (c) { g() }   — nothing follows: `if (!c) return; g()`
        let body = vec![call("f"), iff("c", vec![call("g")], vec![])];
        let cd = ControlDeps::build(&body);
        let c = cd.node(&body[1]).unwrap();
        assert!(cd.is_guard(c, Arm::Then));
    }

    #[test]
    fn continue_is_not_a_required_check() {
        let lp = Stmt::Loop { binding: None, iter: None, body: vec![iff("skip", vec![Stmt::Jump(loc())], vec![]), call("f")], loc: loc() };
        let body = vec![lp];
        let cd = ControlDeps::build(&body);
        let Stmt::Loop { body: lb, .. } = &body[0] else { unreachable!() };
        let skip = cd.node(&lb[0]).unwrap();
        assert!(!cd.is_guard(skip, Arm::Else));
    }

    #[test]
    fn a_nested_early_return_guards_both_conditions() {
        // if (a) { log(); if (b) return }; f()
        let body = vec![iff("a", vec![call("log"), iff("b", vec![ret()], vec![])], vec![]), call("f")];
        let cd = ControlDeps::build(&body);
        let a = cd.node(&body[0]).unwrap();
        let Stmt::If { then, .. } = &body[0] else { unreachable!() };
        let b = cd.node(&then[1]).unwrap();
        assert_eq!(deps_of(&cd, &body[1]), vec![(a, Arm::Else), (b, Arm::Else)]);
        // `¬b` is required wherever the check sits; `a` is an alternative (its then side goes on).
        assert!(cd.is_guard(b, Arm::Else));
        assert!(!cd.is_guard(a, Arm::Else) && !cd.is_guard(a, Arm::Then));
        assert_eq!(deps_of(&cd, &then[0]), vec![(a, Arm::Then)]);
    }

    #[test]
    fn continue_conditions_the_rest_of_the_loop_body() {
        // for (x of xs) { if (skip) continue; f() }; g()
        let lp = Stmt::Loop { binding: None, iter: None, body: vec![iff("skip", vec![Stmt::Jump(loc())], vec![]), call("f")], loc: loc() };
        let body = vec![lp, call("g")];
        let cd = ControlDeps::build(&body);
        let Stmt::Loop { body: lb, .. } = &body[0] else { unreachable!() };
        let skip = cd.node(&lb[0]).unwrap();
        assert_eq!(deps_of(&cd, &lb[1]), vec![(skip, Arm::Else)]);
        // After the loop: always runs.
        assert_eq!(deps_of(&cd, &body[1]), vec![(ENTRY, Arm::Plain)]);
    }

    #[test]
    fn reaching_definitions_follow_branches_and_loops() {
        // let s = a; if (c) { s = b }; use(s); for (x of xs) { s = x }; use(s)
        let assign =
            |v: &str| Stmt::Expr(Expr::Assign { target: Box::new(Expr::Ident("s".into())), value: Box::new(Expr::Ident(v.into())), loc: loc() }, loc());
        let body = vec![
            Stmt::Let { name: "s".into(), ty: None, init: Some(Expr::Ident("a".into())), loc: loc() },
            iff("c", vec![assign("b")], vec![]),
            call("use1"),
            Stmt::Loop { binding: Some("x".into()), iter: None, body: vec![assign("x")], loc: loc() },
            call("use2"),
        ];
        let cd = ControlDeps::build(&body);
        let n = |s: &Stmt| cd.node(s).unwrap();
        let Stmt::If { then, .. } = &body[1] else { unreachable!() };
        let Stmt::Loop { body: lb, .. } = &body[3] else { unreachable!() };
        let mut at_use1 = cd.reaching(n(&body[2]), "s").to_vec();
        at_use1.sort();
        let mut want1 = vec![n(&body[0]), n(&then[0])];
        want1.sort();
        assert_eq!(at_use1, want1);
        let mut at_use2 = cd.reaching(n(&body[4]), "s").to_vec();
        at_use2.sort();
        let mut want2 = vec![n(&body[0]), n(&then[0]), n(&lb[0])];
        want2.sort();
        assert_eq!(at_use2, want2);
        assert!(cd.reaching(n(&body[0]), "s").is_empty());
    }

    #[test]
    fn break_in_a_switch_block_ends_the_case_only() {
        // { if (k==a) { x(); break } if (k==b) { y() } }; z()
        let sw = Stmt::Block(vec![iff("isA", vec![call("x"), Stmt::Jump(loc())], vec![]), iff("isB", vec![call("y")], vec![])]);
        let body = vec![sw, call("z")];
        let cd = ControlDeps::build(&body);
        let Stmt::Block(cases) = &body[0] else { unreachable!() };
        let a = cd.node(&cases[0]).unwrap();
        assert_eq!(deps_of(&cd, &cases[1]), vec![(a, Arm::Else)]);
        assert_eq!(deps_of(&cd, &body[1]), vec![(ENTRY, Arm::Plain)]);
    }
}
