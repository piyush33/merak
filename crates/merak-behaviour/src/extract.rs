//! Per-entity fact extraction: walks each lowered function body and records
//! calls, effects, reads, writes, guards, subscriptions and routes.

use crate::catalog::{self, CallSite, Catalog};
use crate::model::*;
use crate::pred::Pred;
use crate::resolve::{render, Callee, Env, Index, Ty};
use merak_ir::{self as ir, Expr, Loc, Stmt};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Typing environments of every function, plus what callbacks were learned from their call sites.
#[derive(Debug, Default)]
pub struct Envs {
    pub by_fn: HashMap<String, Env>,
    /// Closures passed to a query-filter method (`where((eb) => …)`). Their predicate
    /// is recorded at the call site, rendered from what the closure returns.
    pub filter_callbacks: HashSet<String>,
    /// Types for the untyped first parameter of closures, from the catalog's callback rules.
    hints: HashMap<String, Ty>,
}

/// Build typing environments for every function (closures inherit their parent's).
pub fn build_envs(ix: &Index, cat: &Catalog) -> Envs {
    let mut envs = Envs::default();
    let ids: Vec<String> = ix.program.functions().map(|f| f.id.clone()).collect();
    for id in ids {
        env_for(ix, cat, &id, &mut envs);
    }
    envs
}

fn env_for(ix: &Index, cat: &Catalog, id: &str, envs: &mut Envs) -> Env {
    if let Some(e) = envs.by_fn.get(id) {
        return e.clone();
    }
    let Some(f) = ix.function(id) else { return Env::default() };
    let module = id.split("::").next().unwrap_or("").to_string();
    let mut env = match &f.parent {
        Some(p) => env_for(ix, cat, p, envs),
        None => Env { module: module.clone(), ..Default::default() },
    };
    if f.class.is_some() {
        env.this_class = Some(format!("{module}::{}", f.class.as_deref().unwrap_or("")));
    }
    for (i, p) in f.params.iter().enumerate() {
        let ty = match &p.ty {
            Some(t) => ix.resolve_type(&module, t),
            None if i == 0 => envs.hints.get(id).cloned().unwrap_or(Ty::Unknown),
            None => Ty::Unknown,
        };
        env.vars.insert(p.name.clone(), ty);
    }
    collect_locals(ix, &f.body, &mut env);
    constant_locals(ix, &f.body, &mut env);
    type_callbacks(ix, cat, &env, &f.body, envs);
    envs.by_fn.insert(id.to_string(), env.clone());
    env
}

/// Record parameter types for closures passed to catalogued external methods,
/// e.g. `qb` in `query.$if(cond, (qb) => qb.where(…))` is the query builder.
fn type_callbacks(ix: &Index, cat: &Catalog, env: &Env, body: &[Stmt], envs: &mut Envs) {
    let mut found = vec![];
    visit_calls(body, &mut |c| {
        // Closures passed directly, or through a local (`const p = (eb) => …; qb.where(p)`).
        let closures: Vec<(usize, String)> = c
            .args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                Expr::Closure(id) => Some((i, id.clone())),
                Expr::Ident(n) => match env.vars.get(n) {
                    Some(Ty::Function(id)) if ix.function(id).is_some_and(|f| f.parent.is_some()) => Some((i, id.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        if closures.is_empty() {
            return;
        }
        let Callee::External(api) = ix.resolve_call(env, &c.callee) else { return };
        for (i, id) in closures {
            let Some(rule) = cat.callback_for(&api, i) else { continue };
            let ty = match (rule.param.as_str(), &c.callee) {
                ("receiver", Expr::Member { object, .. }) => ix.type_of(env, object),
                ("receiver", _) => Ty::Unknown,
                (canon, _) => Ty::External(canon.to_string()),
            };
            found.push((id, ty, cat.is_filter(&api)));
        }
    });
    for (id, ty, filter) in found {
        if filter {
            envs.filter_callbacks.insert(id.clone());
        }
        envs.hints.entry(id).or_insert(ty);
    }
}

/// Every call in `stmts`, outermost first (closure bodies are separate functions).
fn visit_calls<'e>(stmts: &'e [Stmt], f: &mut dyn FnMut(&'e ir::Call)) {
    for s in stmts {
        match s {
            Stmt::Expr(e, _) | Stmt::Throw(e, _) | Stmt::Return(Some(e), _) | Stmt::Let { init: Some(e), .. } => visit_expr(e, f),
            Stmt::If { test, then, otherwise, .. } => {
                visit_expr(test, f);
                visit_calls(then, f);
                visit_calls(otherwise, f);
            }
            Stmt::Loop { iter, body, .. } => {
                if let Some(it) = iter {
                    visit_expr(it, f);
                }
                visit_calls(body, f);
            }
            Stmt::Try { body, handler, finalizer, .. } => {
                visit_calls(body, f);
                visit_calls(handler, f);
                visit_calls(finalizer, f);
            }
            Stmt::Block(b) => visit_calls(b, f),
            Stmt::Return(None, _) | Stmt::Let { init: None, .. } | Stmt::Jump(_) => {}
        }
    }
}

fn visit_expr<'e>(e: &'e Expr, f: &mut dyn FnMut(&'e ir::Call)) {
    match e {
        Expr::Call(c) => {
            f(c);
            visit_expr(&c.callee, f);
            c.args.iter().for_each(|a| visit_expr(a, f));
        }
        Expr::Member { object, .. } => visit_expr(object, f),
        Expr::Index { object, index } => {
            visit_expr(object, f);
            visit_expr(index, f);
        }
        Expr::New { callee, args, .. } => {
            visit_expr(callee, f);
            args.iter().for_each(|a| visit_expr(a, f));
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            visit_expr(left, f);
            visit_expr(right, f);
        }
        Expr::Not(x) | Expr::Await(x) | Expr::Spawn(x) => visit_expr(x, f),
        Expr::Template { exprs: xs, .. } | Expr::Array(xs) => xs.iter().for_each(|x| visit_expr(x, f)),
        Expr::Object(props) => props.iter().for_each(|(_, v)| visit_expr(v, f)),
        Expr::Conditional { test, then, otherwise } => {
            visit_expr(test, f);
            visit_expr(then, f);
            visit_expr(otherwise, f);
        }
        Expr::Assign { target, value, .. } => {
            visit_expr(target, f);
            visit_expr(value, f);
        }
        Expr::Jsx(j) => {
            j.attrs.iter().for_each(|(_, v)| visit_expr(v, f));
            j.children.iter().for_each(|c| visit_expr(c, f));
        }
        _ => {}
    }
}

/// What markup renders: each piece of content with the element around it and the
/// conditions (`a && <x/>`, `a ? <x/> : <y/>`) that show it.
fn rendered(e: &Expr, style: &str, when: &mut Vec<String>, out: &mut Vec<Rendered>) {
    let cond = |w: &Vec<String>| w.join(" ∧ ");
    match e {
        Expr::Jsx(j) => {
            let style = if j.tag.is_empty() {
                style.to_string()
            } else {
                let classes: String = j.class().map(|c| c.split_whitespace().map(|x| format!(".{x}")).collect()).unwrap_or_default();
                format!("{}{classes}", j.tag)
            };
            if j.children.is_empty() && !j.tag.is_empty() {
                out.push(Rendered { content: format!("<{}>", j.tag), style: style.clone(), when: cond(when) });
            }
            for c in &j.children {
                rendered(c, &style, when, out);
            }
        }
        Expr::Logical { op: ir::LogicOp::And, left, right } if contains_jsx(right) => {
            when.push(render(left));
            rendered(right, style, when, out);
            when.pop();
        }
        Expr::Conditional { test, then, otherwise } if contains_jsx(then) || contains_jsx(otherwise) => {
            when.push(render(test));
            rendered(then, style, when, out);
            when.pop();
            when.push(format!("!({})", render(test)));
            rendered(otherwise, style, when, out);
            when.pop();
        }
        Expr::Str(s) if !style.is_empty() => out.push(Rendered { content: format!("{s:?}"), style: style.to_string(), when: cond(when) }),
        Expr::Null | Expr::Undefined | Expr::Bool(_) => {}
        other if !style.is_empty() => out.push(Rendered { content: format!("{{{}}}", render(other)), style: style.to_string(), when: cond(when) }),
        _ => {}
    }
}

/// A type as written: `Promise<Order>`, `string[]`, `"ADMIN" | "MANAGER"`.
pub fn type_text(t: &ir::TypeRef) -> String {
    match t {
        ir::TypeRef::Named(n, args) if args.is_empty() => n.clone(),
        ir::TypeRef::Named(n, args) => format!("{n}<{}>", args.iter().map(type_text).collect::<Vec<_>>().join(", ")),
        ir::TypeRef::Array(el) => format!("{}[]", type_text(el)),
        ir::TypeRef::StringLiterals(vs) => vs.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(" | "),
        ir::TypeRef::Union(ts) => ts.iter().map(type_text).collect::<Vec<_>>().join(" | "),
        ir::TypeRef::Object(fs) => format!("{{ {} }}", fs.iter().map(|(k, v)| format!("{k}: {}", type_text(v))).collect::<Vec<_>>().join("; ")),
        ir::TypeRef::Primitive(p) => p.clone(),
        ir::TypeRef::Unknown => "?".into(),
    }
}

fn contains_jsx(e: &Expr) -> bool {
    match e {
        Expr::Jsx(_) => true,
        Expr::Logical { right, .. } => contains_jsx(right),
        Expr::Conditional { then, otherwise, .. } => contains_jsx(then) || contains_jsx(otherwise),
        _ => false,
    }
}

/// Locals bound once to a constant (`query := "SELECT …"`, `const url = `${API}/x``) and
/// never reassigned: their value is known wherever they are used.
fn constant_locals(ix: &Index, body: &[Stmt], env: &mut Env) {
    let mut lets: HashMap<String, Vec<&Expr>> = HashMap::new();
    let mut assigned: HashSet<String> = HashSet::new();
    fn walk<'e>(stmts: &'e [Stmt], lets: &mut HashMap<String, Vec<&'e Expr>>, assigned: &mut HashSet<String>) {
        for s in stmts {
            match s {
                Stmt::Let { name, init, .. } => {
                    lets.entry(name.clone()).or_default().extend(init.iter());
                    if init.is_none() {
                        assigned.insert(name.clone());
                    }
                }
                Stmt::Expr(Expr::Assign { target, .. }, _) => {
                    if let Expr::Ident(n) = target.as_ref() {
                        assigned.insert(n.clone());
                    }
                }
                Stmt::If { then, otherwise, .. } => {
                    walk(then, lets, assigned);
                    walk(otherwise, lets, assigned);
                }
                Stmt::Loop { binding, body, .. } => {
                    assigned.extend(binding.iter().cloned());
                    walk(body, lets, assigned);
                }
                Stmt::Try { body, handler, finalizer, .. } => {
                    walk(body, lets, assigned);
                    walk(handler, lets, assigned);
                    walk(finalizer, lets, assigned);
                }
                Stmt::Block(b) => walk(b, lets, assigned),
                _ => {}
            }
        }
    }
    walk(body, &mut lets, &mut assigned);
    for (name, inits) in lets {
        if let ([init], false) = (inits.as_slice(), assigned.contains(&name)) {
            if ix.string_skeleton(env, init).is_some() {
                env.consts.insert(name, (*init).clone());
            }
        }
    }
}

fn collect_locals(ix: &Index, stmts: &[Stmt], env: &mut Env) {
    for s in stmts {
        match s {
            Stmt::Let { name, ty, init, .. } => {
                let declared = ty.as_ref().map(|t| ix.resolve_type(&env.module, t)).unwrap_or(Ty::Unknown);
                let t = if declared != Ty::Unknown { declared } else { init.as_ref().map(|e| ix.type_of(env, e)).unwrap_or(Ty::Unknown) };
                env.vars.insert(name.clone(), t);
            }
            Stmt::If { then, otherwise, .. } => {
                collect_locals(ix, then, env);
                collect_locals(ix, otherwise, env);
            }
            Stmt::Loop { binding, iter, body, .. } => {
                if let (Some(b), Some(it)) = (binding, iter) {
                    let el = match ix.type_of(env, it) {
                        Ty::Array(el) => *el,
                        _ => Ty::Unknown,
                    };
                    env.vars.insert(b.clone(), el);
                }
                collect_locals(ix, body, env);
            }
            Stmt::Try { body, handler, finalizer, .. } => {
                collect_locals(ix, body, env);
                collect_locals(ix, handler, env);
                collect_locals(ix, finalizer, env);
            }
            Stmt::Block(b) => collect_locals(ix, b, env),
            _ => {}
        }
    }
}

pub fn extract_entity(ix: &Index, cat: &Catalog, envs: &Envs, f: &ir::Function) -> Entity {
    let module = f.id.split("::").next().unwrap_or("").to_string();
    let fallback = Env::default();
    let mut w = Walker {
        ix,
        cat,
        env: envs.by_fn.get(&f.id).unwrap_or(&fallback),
        envs,
        depth: 0,
        // A filter callback's predicate belongs to the call it is passed to.
        filter_depth: u32::from(envs.filter_callbacks.contains(&f.id)),
        request_depth: 0,
        spawn_depth: 0,
        defs: HashMap::new(),
        def_exprs: HashMap::new(),
        made_by: HashMap::new(),
        ctes: scope_ctes(ix, f),
        path: vec![],
        consumed_closures: BTreeSet::new(),
        clause: clause_shape(ix, envs, &f.id, 0),
        e: Entity {
            id: f.id.clone(),
            name: f.name.clone(),
            kind: match f.kind {
                ir::FunctionKind::Function => EntityKind::Function,
                ir::FunctionKind::Method => EntityKind::Method,
                ir::FunctionKind::Constructor => EntityKind::Constructor,
                ir::FunctionKind::Closure => EntityKind::Closure,
            },
            module,
            class: f.class.clone(),
            parent: f.parent.clone(),
            loc: f.loc.clone(),
            end_line: f.end_line,
            body_hash: f.body_hash.clone(),
            calls: vec![],
            effects: vec![],
            reads: BTreeMap::new(),
            writes: vec![],
            guards: vec![],
            returns: None,
            returns_at: None,
            outputs: vec![],
            params: f
                .params
                .iter()
                .map(|p| match &p.ty {
                    Some(t) => format!("{}: {}", p.name, type_text(t)),
                    None => p.name.clone(),
                })
                .collect(),
            throws: false,
            subscriptions: vec![],
            routes: vec![],
            unknowns: vec![],
            call_shapes: vec![],
            filters: vec![],
            logs: vec![],
            consts: BTreeMap::new(),
            skips: vec![],
            access: vec![],
        },
    };
    w.stmts(&f.body, true);
    w.decorators(f);
    if is_predicate_fn(f) {
        let returns: Vec<(&Expr, &Loc)> = f
            .body
            .iter()
            .filter_map(|s| match s {
                Stmt::Return(Some(e), l) => Some((e, l)),
                _ => None,
            })
            .collect();
        // `return result` of a local says nothing a caller can read.
        let local = |e: &Expr| matches!(e, Expr::Ident(n) if w.env.vars.contains_key(n) && !f.params.iter().any(|p| &p.name == n));
        if let [(e, l)] = returns.as_slice() {
            if local(e) {
                return w.e;
            }
            w.e.returns = Some(w.pred(e));
            w.e.returns_at = Some((*l).clone());
        }
    }
    w.e
}

fn is_predicate_fn(f: &ir::Function) -> bool {
    let boolean = matches!(f.return_type.as_ref().map(|t| t.unwrap_async()), Some(ir::TypeRef::Primitive(p)) if p == "boolean");
    let named = ["can", "is", "has", "should", "may", "allow"]
        .iter()
        .any(|p| f.name.starts_with(p) && f.name[p.len()..].chars().next().is_some_and(|c| c.is_uppercase()));
    boolean || (named && f.return_type.is_none())
}

struct Walker<'a, 'p> {
    ix: &'a Index<'p>,
    cat: &'a Catalog,
    env: &'a Env,
    envs: &'a Envs,
    /// Nesting depth of conditional/loop/try regions.
    depth: u32,
    /// Nesting depth inside query-filter arguments: only the outermost filter is recorded.
    filter_depth: u32,
    /// Inside the arguments of an HTTP call: serialising its body is part of the request.
    request_depth: u32,
    /// Inside `go …`: calls start concurrently.
    spawn_depth: u32,
    /// How each local was computed so far, constants by value: `0.25 * log1p(breadth); +6 when …`.
    defs: HashMap<String, String>,
    /// The same, as expressions (other locals substituted), where a local has one value.
    def_exprs: HashMap<String, Expr>,
    /// Program functions whose results flow into each local, on any path: `predicate` ← `scopePredicate`.
    made_by: HashMap<String, BTreeSet<String>>,
    /// CTE names in scope: reading one is not a table read.
    ctes: BTreeSet<String>,
    /// Conditions under which the current statement runs: enclosing `if` tests and the
    /// negations of earlier early exits. Part of every call shape, so a condition added
    /// around a call is a change, while `if (c) { f() }` and `if (!c) return; f()` agree.
    /// Top-level guards are marked (`true`): guard facts already compare them.
    path: Vec<(Pred, bool)>,
    consumed_closures: BTreeSet<String>,
    /// What this function builds, when it assembles a string clause: `⟨a⟩ AND ⟨b, each c in cs⟩`.
    clause: Option<String>,
    e: Entity,
}

impl Walker<'_, '_> {
    fn stmts(&mut self, stmts: &[Stmt], top: bool) {
        let scope = self.path.len();
        for (i, s) in stmts.iter().enumerate() {
            // At function top level, a trailing `if (c) { … }` (no else, no exit inside)
            // is the same as `if (!c) return; …`: record the guard, walk the body as top level.
            if let (true, true, Stmt::If { test, then, otherwise, loc }) = (top, i + 1 == stmts.len(), s) {
                if otherwise.is_empty() && !matches!(then.last(), Some(Stmt::Throw(..) | Stmt::Return(..))) {
                    self.expr(test, loc);
                    let requires = self.pred(test);
                    let requires = self.with_domain(requires);
                    self.e.guards.push(GuardFact { requires, on_fail: OnFail::Return, loc: loc.clone(), returns_value: false });
                    self.path.push((self.pred(test), true));
                    self.stmts(then, true);
                    continue;
                }
            }
            self.stmt(s, top);
            // After `if (c) return|throw|continue;` the rest of the block runs under ¬c
            // (after a top-level return/throw, a guard).
            if let Stmt::If { test, then, otherwise, .. } = s {
                if otherwise.is_empty() && matches!(then.last(), Some(Stmt::Throw(..) | Stmt::Return(..) | Stmt::Jump(..))) {
                    let guard = top && !matches!(then.last(), Some(Stmt::Jump(..)));
                    self.path.push((self.pred(test).negate(), guard));
                }
            }
        }
        self.path.truncate(scope);
    }

    fn stmt(&mut self, s: &Stmt, top: bool) {
        match s {
            Stmt::Expr(e, loc) => self.expr(e, loc),
            Stmt::Let { name, ty, init, loc } => {
                if let Some(init) = init {
                    let m = self.makers(init);
                    self.made_by.entry(name.clone()).or_default().extend(m);
                    let value = self.inline_consts(init);
                    let text = render(&value);
                    // Long definitions are not substituted further: formulas stay readable.
                    if text.chars().count() <= 300 {
                        self.def_exprs.insert(name.clone(), value);
                    } else {
                        self.def_exprs.remove(name);
                    }
                    self.defs.insert(name.clone(), text);
                    self.expr(init, loc);
                    // `const order: Order = { status: …, … }` constructs a record.
                    if let (Some(ty), Expr::Object(props)) = (ty, init) {
                        if let Ty::Record { name, .. } = self.ix.resolve_type(&self.env.module, ty) {
                            for (k, v) in props {
                                if k == "..." {
                                    continue;
                                }
                                let value = self.ix.const_str(self.env, v);
                                self.e.writes.push(WriteFact { field: format!("{name}.{k}"), value, creation: true, loc: loc.clone() });
                            }
                        }
                    }
                }
            }
            Stmt::If { test, then, otherwise, loc } => {
                self.expr(test, loc);
                let exits = match then.last() {
                    Some(Stmt::Throw(..)) => Some(OnFail::Throw),
                    Some(Stmt::Return(..)) => Some(OnFail::Return),
                    _ => None,
                };
                if top && otherwise.is_empty() {
                    if let Some(on_fail) = exits {
                        let requires = self.pred(test).negate();
                        let requires = self.with_domain(requires);
                        let returns_value = matches!(then.last(), Some(Stmt::Return(Some(_), _)));
                        self.e.guards.push(GuardFact { requires, on_fail, loc: loc.clone(), returns_value });
                    }
                }
                self.depth += 1;
                let cond = self.pred(test);
                self.path.push((cond.clone(), false));
                self.stmts(then, false);
                self.path.pop();
                self.path.push((cond.negate(), false));
                self.stmts(otherwise, false);
                self.path.pop();
                self.depth -= 1;
            }
            Stmt::Return(e, loc) => {
                if let Some(e) = e {
                    self.expr(e, loc);
                    let mut renders = vec![];
                    rendered(e, "", &mut Vec::new(), &mut renders);
                    // `return s`: what `s` was computed by, when it was computed here.
                    let (value, formula) = match e {
                        // A clause built up from other builders: what it composes.
                        Expr::Ident(_) if self.clause.as_ref().is_some_and(|c| c.contains('⟨')) => (self.clause.clone().unwrap_or_default(), true),
                        Expr::Array(xs) if matches!(xs.first(), Some(Expr::Ident(_))) && self.clause.as_ref().is_some_and(|c| c.contains('⟨')) => {
                            let rest: Vec<String> = xs.iter().skip(1).map(render).collect();
                            (format!("[{}]", std::iter::once(self.clause.clone().unwrap_or_default()).chain(rest).collect::<Vec<_>>().join(", ")), true)
                        }
                        Expr::Ident(x) if self.defs.get(x).is_some_and(|d| is_formula(d)) => (self.defs[x].clone(), true),
                        // `return clause, args, true` (Go): each local by what it was built from.
                        Expr::Array(xs) if xs.iter().any(|x| matches!(x, Expr::Ident(n) if self.defs.get(n).is_some_and(|d| is_formula(d)))) => {
                            let parts: Vec<String> = xs
                                .iter()
                                .map(|x| match x {
                                    Expr::Ident(n) if self.defs.get(n).is_some_and(|d| is_formula(d)) => self.defs[n].clone(),
                                    x => render(x),
                                })
                                .collect();
                            (format!("[{}]", parts.join(", ")), true)
                        }
                        e => (render(e), false),
                    };
                    self.e.outputs.push(Output { when: self.conditions(false), value, renders, loc: loc.clone(), formula });
                }
            }
            Stmt::Throw(e, loc) => {
                self.e.throws = true;
                // The error thrown is part of the guard that throws it, not a call shape.
                match e {
                    Expr::New { args, loc: nloc, .. } => args.iter().for_each(|a| self.expr(a, nloc)),
                    e => self.expr(e, loc),
                }
            }
            Stmt::Loop { binding, iter, body, loc } => {
                if let Some(it) = iter {
                    self.expr(it, loc);
                }
                // `if c { continue }` at the top of the body: which items the loop leaves out.
                let over = match (binding, iter) {
                    (Some(b), Some(it)) => format!("{b} in {}", render(it)),
                    _ => "loop".to_string(),
                };
                self.loop_skips(body, &over);
                self.depth += 1;
                self.stmts(body, false);
                self.depth -= 1;
            }
            Stmt::Try { body, handler, finalizer, .. } => {
                self.stmts(body, false);
                self.depth += 1;
                self.stmts(handler, false);
                self.depth -= 1;
                self.stmts(finalizer, false);
            }
            Stmt::Block(b) => self.stmts(b, top),
            Stmt::Jump(_) => {}
        }
    }

    fn expr(&mut self, e: &Expr, loc: &Loc) {
        match e {
            Expr::Call(c) => self.call(c),
            Expr::Member { object, property } => {
                if let Some(path) = self.ix.field_path(self.env, object, property) {
                    self.e.reads.entry(path).or_insert_with(|| loc.clone());
                }
                self.expr(object, loc);
            }
            Expr::Assign { target, value, loc: aloc } => {
                // `s += x` / `s -= x` / `s = x` on a local: the formula grows, with its condition.
                if let Expr::Ident(x) = target.as_ref() {
                    let m = self.makers(value);
                    self.made_by.entry(x.clone()).or_default().extend(m);
                    let when = self.conditions(false);
                    let when = if when.is_empty() { String::new() } else { format!(" when {}", when.join(" ∧ ")) };
                    let step = match value.as_ref() {
                        Expr::Binary { op: op @ (ir::BinOp::Add | ir::BinOp::Sub), left, right } if matches!(left.as_ref(), Expr::Ident(l) if l == x) => {
                            let sign = if *op == ir::BinOp::Add { "+" } else { "−" };
                            Some(format!("{sign}{}{when}", render(&self.inline_consts(right))))
                        }
                        v if when.is_empty() => {
                            let value = self.inline_consts(v);
                            let text = render(&value);
                            if text.chars().count() <= 300 {
                                self.def_exprs.insert(x.clone(), value);
                            } else {
                                self.def_exprs.remove(x);
                            }
                            self.defs.insert(x.clone(), text);
                            None
                        }
                        v => {
                            // Set on some paths only: no single value to substitute.
                            self.def_exprs.remove(x);
                            Some(format!("= {}{when}", render(&self.inline_consts(v))))
                        }
                    };
                    if let (Some(step), Some(d)) = (step, self.defs.get_mut(x)) {
                        d.push_str(&format!("; {step}"));
                    }
                }
                if let Expr::Member { object, property } = target.as_ref() {
                    if let Some(field) = self.ix.field_path(self.env, object, property) {
                        let v = self.ix.const_str(self.env, value);
                        self.e.writes.push(WriteFact { field, value: v, creation: false, loc: aloc.clone() });
                    }
                    self.expr(object, aloc);
                }
                self.expr(value, aloc);
            }
            Expr::New { callee, args, loc: nloc } => {
                // `Order{Status: StatusPending}` (Go) constructs a record: its data fields are
                // written, and compared as writes. The construction itself stays a call shape,
                // without those fields, so the conditions it runs under still count.
                let mut written = BTreeSet::new();
                if let [Expr::Object(props)] = args.as_slice() {
                    if matches!(self.ix.type_of(self.env, e), Ty::Class(_)) {
                        for (k, v) in props {
                            if let Some(field) = self.ix.field_path(self.env, e, k) {
                                let value = self.ix.const_str(self.env, v);
                                self.e.writes.push(WriteFact { field, value, creation: true, loc: nloc.clone() });
                                written.insert(k.clone());
                            }
                        }
                    }
                }
                // `new X(…)` is a call too: `await new Promise((r) => setTimeout(r, 1000))`.
                let shape_args = match args.as_slice() {
                    [Expr::Object(props)] if !written.is_empty() => {
                        vec![Expr::Object(props.iter().filter(|(k, _)| !written.contains(k)).cloned().collect())]
                    }
                    _ => args.clone(),
                };
                let call = ir::Call { callee: callee.as_ref().clone(), args: shape_args, loc: nloc.clone() };
                let shape = format!("new {}", self.call_shape(&call));
                self.e.call_shapes.push(CallShape { shape, target: None, when: self.conditions(false), guards: self.conditions(true), loc: nloc.clone(), effect: false });
                args.iter().for_each(|a| self.expr(a, nloc));
            }
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left, loc);
                self.expr(right, loc);
            }
            Expr::Not(x) | Expr::Await(x) => self.expr(x, loc),
            // A module constant used here: its value is part of what the entity does.
            Expr::Ident(n) if !self.env.vars.contains_key(n) => {
                if let Some(v) = self.ix.const_str(self.env, e).filter(|v| v.parse::<f64>().is_ok() || v == "true" || v == "false") {
                    self.e.consts.insert(n.clone(), v);
                }
            }
            Expr::Spawn(x) => {
                self.spawn_depth += 1;
                self.expr(x, loc);
                self.spawn_depth -= 1;
            }
            Expr::Index { object, index } => {
                self.expr(object, loc);
                self.expr(index, loc);
            }
            Expr::Template { exprs, .. } => exprs.iter().for_each(|x| self.expr(x, loc)),
            Expr::Object(props) => props.iter().for_each(|(_, v)| self.expr(v, loc)),
            Expr::Array(xs) => xs.iter().for_each(|x| self.expr(x, loc)),
            Expr::Conditional { test, then, otherwise } => {
                self.expr(test, loc);
                self.depth += 1;
                self.expr(then, loc);
                self.expr(otherwise, loc);
                self.depth -= 1;
            }
            Expr::Jsx(j) => {
                j.attrs.iter().for_each(|(_, v)| self.expr(v, loc));
                j.children.iter().for_each(|c| self.expr(c, loc));
            }
            Expr::Closure(id)
                // A closure used as a value (not registered as a handler/route) is
                // assumed to run on behalf of this entity.
                if !self.consumed_closures.contains(id) => {
                    self.e.calls.push(CallFact { target: id.clone(), loc: loc.clone(), conditional: true, spawned: self.spawn_depth > 0 });
                    // By its name within this entity, so the conditions it is used under count.
                    // By kind, not position: inserting a sibling renumbers `useMemo#4` to `#5`.
                    let name = id.strip_prefix(&format!("{}.", self.e.id)).unwrap_or(id);
                    let name = name.split('#').next().unwrap_or(name);
                    let shape = format!("<fn {name}>");
                    self.e.call_shapes.push(CallShape { shape, target: Some(id.clone()), when: self.conditions(false), guards: self.conditions(true), loc: loc.clone(), effect: false });
                }
            _ => {}
        }
    }

    fn call(&mut self, c: &ir::Call) {
        let loc = &c.loc;
        self.access_args(&c.args, loc);
        let shape = self.call_shape(c);
        if self.api_name(&c.callee).is_some_and(|api| self.cat.is_log(&api)) {
            self.e.logs.push(CallShape { shape, target: None, when: self.conditions(false), guards: self.conditions(true), loc: loc.clone(), effect: false });
            c.args.iter().for_each(|a| self.expr(a, loc));
            return;
        }
        match self.ix.resolve_call(self.env, &c.callee) {
            Callee::Internal(target) => {
                self.e.call_shapes.push(CallShape {
                    shape,
                    target: Some(target.clone()),
                    when: self.conditions(false),
                    guards: self.conditions(true),
                    loc: loc.clone(),
                    effect: false,
                });
                self.e.calls.push(CallFact { target, loc: loc.clone(), conditional: self.depth > 0, spawned: self.spawn_depth > 0 });
            }
            Callee::External(api) if self.cat.is_filter(&api) => {
                if self.filter_depth == 0 {
                    let expr = self.filter_text(self.env, c);
                    self.e.filters.push(QueryFilter { expr, loc: loc.clone() });
                }
                // The builder chain is walked as usual; nested predicates are part of this one.
                if let Expr::Member { object, .. } = &c.callee {
                    self.expr(object, loc);
                }
                self.filter_depth += 1;
                c.args.iter().for_each(|a| self.expr(a, loc));
                self.filter_depth -= 1;
                return;
            }
            // A literal inside a predicate is part of the predicate's text.
            Callee::External(api) if self.filter_depth > 0 && self.cat.is_literal(&api) => {}
            // `JSON.stringify` of a request body is part of the request (its payload).
            Callee::External(api) if self.request_depth > 0 && api == "JSON.stringify()" => {}
            // Running a query (`…selectFrom(…).where(…).execute()`) is its effect, already recorded.
            Callee::External(api) if self.runs_effect_chain(&api) => {}
            // A builder call that hands over a callback (`$if(c, (qb) => …)`, `select((eb) => …)`)
            // is described by the callback's own facts and calls.
            Callee::External(api) if c.args.iter().enumerate().any(|(i, a)| matches!(a, Expr::Closure(_)) && self.cat.callback_for(&api, i).is_some()) => {}
            Callee::External(api) => {
                let effect = self.cat.effect_for(&api).is_some();
                if !self.external_call(&api, c) || effect {
                    self.e.call_shapes.push(CallShape {
                        shape,
                        target: None,
                        when: self.conditions(false),
                        guards: self.conditions(true),
                        loc: loc.clone(),
                        effect,
                    });
                }
            }
            Callee::Unresolved(text) => {
                self.e.call_shapes.push(CallShape {
                    shape,
                    target: None,
                    when: self.conditions(false),
                    guards: self.conditions(true),
                    loc: loc.clone(),
                    effect: false,
                });
                self.e.unknowns.push(Unknown { question: format!("unresolved call `{text}`"), loc: loc.clone() });
            }
        }
        if let Expr::Member { object, .. } = &c.callee {
            self.expr(object, loc);
        }
        let request = self.api_name(&c.callee).is_some_and(|api| self.cat.effect_for(&api).is_some_and(|r| r.kind == "http"));
        self.request_depth += u32::from(request);
        for a in &c.args {
            self.expr(a, loc);
        }
        self.request_depth -= u32::from(request);
    }

    /// `target(args)`: the target by short name (stable across moves), constant
    /// arguments by value, object literals by key (and constant value), the rest `_`.
    fn call_shape(&self, c: &ir::Call) -> String {
        let target = match self.ix.resolve_call(self.env, &c.callee) {
            // A closure called in place (`go func() { … }()`): by kind, not position.
            Callee::Internal(id) if matches!(c.callee, Expr::Closure(_)) => {
                let name = id.strip_prefix(&format!("{}.", self.e.id)).unwrap_or(&id);
                format!("<fn {}>", name.split('#').next().unwrap_or(name))
            }
            Callee::Internal(id) => id.rsplit("::").next().unwrap_or(&id).to_string(),
            // Fluent chains by root and last method, so inserting a `.where()`
            // changes one shape rather than every later link of the chain.
            Callee::External(canon) => match canon.trim_end_matches("()").split("().").collect::<Vec<_>>().as_slice() {
                [root, .., last] => format!("{root}()…{last}"),
                _ => canon.trim_end_matches("()").to_string(),
            },
            Callee::Unresolved(text) => text,
        };
        let arg = |a: &Expr| match (self.ix.const_str(self.env, a), a) {
            // Numbers, booleans and null as written; strings quoted.
            (Some(v), Expr::Num(_) | Expr::Bool(_) | Expr::Null) => v,
            // An SQL statement by its skeleton: its WHERE clause is compared as query filters.
            (Some(v), _) if crate::sql::parse(&v, &|_| None).is_some() => {
                format!("{:?}", crate::sql::parse(&v, &|_| None).map(|q| q.skeleton).unwrap_or_default())
            }
            (Some(v), _) => format!("{v:?}"),
            (None, Expr::Object(props)) => {
                // Access keys are covered by access facts.
                let mut keys: Vec<String> = props
                    .iter()
                    .filter(|(k, _)| !self.cat.is_access_key(k))
                    .map(|(k, v)| match self.ix.const_str(self.env, v) {
                        Some(v) => format!("{k}={v:?}"),
                        None => k.clone(),
                    })
                    .collect();
                keys.sort();
                format!("{{{}}}", keys.join(", "))
            }
            _ => "_".to_string(),
        };
        format!("{target}({})", c.args.iter().map(arg).collect::<Vec<_>>().join(", "))
    }

    /// The current path conditions (`guards`: the entity's own guards), rendered for a call shape.
    fn conditions(&self, guards: bool) -> Vec<String> {
        let preds: Vec<Pred> = self.path.iter().filter(|(_, g)| *g == guards).map(|(p, _)| p.clone()).collect();
        let mut conds: Vec<String> = match Pred::And(preds).simplify() {
            Pred::And(ps) => ps.iter().map(Pred::render).collect(),
            Pred::True => vec![],
            p => vec![p.render()],
        };
        conds.sort();
        conds.dedup();
        conds
    }

    /// Access requirements in object arguments: `{ auth, permission: Permission.AlbumRead }`.
    fn access_args(&mut self, args: &[Expr], loc: &Loc) {
        for a in args {
            let Expr::Object(props) = a else { continue };
            for (k, v) in props.iter().filter(|(k, _)| self.cat.is_access_key(k)) {
                let values: Vec<String> = match v {
                    Expr::Array(xs) => xs.iter().map(|x| self.ix.const_str(self.env, x).unwrap_or_else(|| "_".into())).collect(),
                    v => vec![self.ix.const_str(self.env, v).unwrap_or_else(|| "_".into())],
                };
                let grant = self.cat.access.grants.contains(k);
                for value in values {
                    self.e.access.push(AccessFact { requirement: format!("{k}={value}"), grant, loc: loc.clone() });
                }
            }
        }
    }

    /// Canonical name of a decorator or call target, for catalog matching.
    fn api_name(&self, callee: &Expr) -> Option<String> {
        match self.ix.resolve_call(self.env, callee) {
            Callee::External(canon) => Some(canon),
            Callee::Internal(id) => Some(format!("{id}()")),
            Callee::Unresolved(_) => None,
        }
    }

    /// Routes and subscriptions declared by decorators on the function (and its class).
    fn decorators(&mut self, f: &ir::Function) {
        let handler = f.id.clone();
        for d in &f.decorators {
            self.access_args(&d.args, &d.loc);
            let Some(api) = self.api_name(&d.callee) else { continue };
            let site = Site { segments: catalog::split_segments(&api), walker: self, call: d };
            if let Some(rule) = self.cat.route_for(&api, true) {
                let method = catalog::eval_spec(&rule.method, &site).unwrap_or_else(|| "?".into());
                let path = catalog::eval_spec(&rule.path, &site);
                let prefix = rule.prefix.as_ref().map(|p| self.class_decorator_arg(f, p));
                let path = match (prefix, path) {
                    (Some(None), _) | (_, None) => "<dynamic>".to_string(),
                    (Some(Some(pre)), Some(p)) => join_route(&pre, &p),
                    (None, Some(p)) => join_route("", &p),
                };
                self.e.routes.push(Route { method, path, handler: handler.clone(), loc: d.loc.clone() });
            } else if let Some(rule) = self.cat.subscription_for(&api, true) {
                let event = catalog::eval_spec(&rule.event, &site).unwrap_or_else(|| "<dynamic>".into());
                self.e.subscriptions.push(Subscription { event, handler: handler.clone(), loc: d.loc.clone() });
            }
        }
    }

    /// Argument 0 of the enclosing class's decorator matching `pattern`: `Some("")` when
    /// there is no such decorator or it has no argument, `None` when it is not a constant.
    fn class_decorator_arg(&self, f: &ir::Function, pattern: &str) -> Option<String> {
        let class = f.class.as_ref().and_then(|c| self.ix.class(&format!("{}::{c}", self.e.module)));
        let found = class.and_then(|c| c.decorators.iter().find(|d| self.api_name(&d.callee).is_some_and(|a| catalog::api_matches(pattern, &a))));
        let Some(d) = found else { return Some(String::new()) };
        match d.args.first() {
            None => Some(String::new()),
            Some(Expr::Object(props)) => props.iter().find(|(k, _)| k == "path").and_then(|(_, v)| self.ix.const_str(self.env, v)),
            Some(a) => self.ix.const_str(self.env, a),
        }
    }

    /// Record what a catalogued external call means; `false` if the catalog does not know it.
    fn external_call(&mut self, api: &str, c: &ir::Call) -> bool {
        enum Found {
            Effect(EffectKey, bool),
            Subscribe(String, i32),
            Route(String, String, i32),
            Nothing,
        }
        if let Some(rule) = self.cat.effect_for(api).filter(|r| r.kind == "sql") {
            self.sql_call(rule.target.clone(), api, c);
            return true;
        }
        let found = {
            let site = Site { segments: catalog::split_segments(api), walker: self, call: c };
            if let Some(rule) = self.cat.effect_for(api) {
                let target = catalog::eval_spec(&rule.target, &site);
                // `db.with('x', …).selectFrom('x')` reads a CTE, not a table: the CTE's
                // own query is recorded where it is defined.
                if target.as_ref().is_some_and(|t| self.ctes.contains(t)) {
                    return true;
                }
                let method = rule.method.as_ref().and_then(|m| catalog::eval_spec(m, &site));
                let dynamic = target.is_none();
                // A target built at runtime still has a readable shape: `…/search`.
                let target = target.or_else(|| {
                    let i: usize = rule.target.split(':').nth(1)?.split(['|', ':']).next()?.parse().ok()?;
                    let skeleton = self.ix.string_skeleton(self.env, c.args.get(i)?)?;
                    (skeleton != "…").then_some(skeleton)
                });
                Found::Effect(EffectKey { kind: rule.kind.clone(), target: target.unwrap_or_else(|| "<dynamic>".into()), method }, dynamic)
            } else if let Some(rule) = self.cat.subscription_for(api, false) {
                Found::Subscribe(catalog::eval_spec(&rule.event, &site).unwrap_or_else(|| "<dynamic>".into()), rule.handler_arg.unwrap_or(-1))
            } else if let Some(rule) = self.cat.route_for(api, false) {
                Found::Route(
                    catalog::eval_spec(&rule.method, &site).unwrap_or_else(|| "?".into()),
                    catalog::eval_spec(&rule.path, &site).unwrap_or_else(|| "<dynamic>".into()),
                    rule.handler_arg.unwrap_or(-1),
                )
            } else {
                Found::Nothing
            }
        };
        match found {
            Found::Effect(key, dynamic) => {
                if dynamic {
                    self.e.unknowns.push(Unknown { question: format!("effect target of `{api}` is not a constant"), loc: c.loc.clone() });
                }
                let payload = if key.kind == "http" { self.request_body(c) } else { None };
                self.e.effects.push(EffectFact { key, api: api.to_string(), payload, loc: c.loc.clone() });
            }
            Found::Subscribe(event, idx) => {
                if let Some(handler) = self.handler_arg(c, idx) {
                    self.e.subscriptions.push(Subscription { event, handler, loc: c.loc.clone() });
                }
            }
            Found::Route(method, path, idx) => {
                if let Some(handler) = self.handler_arg(c, idx) {
                    self.e.routes.push(Route { method, path, handler, loc: c.loc.clone() });
                }
            }
            Found::Nothing => return false,
        }
        true
    }

    /// Raw SQL (`target = "sql:N"`, the statement in argument N): the tables it reads and
    /// writes, its schema changes, its row conditions, and where written columns come from.
    fn sql_call(&mut self, target: String, api: &str, c: &ir::Call) {
        let n: usize = target.strip_prefix("sql:").and_then(|n| n.parse().ok()).unwrap_or(0);
        let text = c.args.get(n).and_then(|a| self.ix.const_str(self.env, a));
        let value = |k: usize| c.args.get(n + k).and_then(|a| self.ix.const_str(self.env, a));
        let is_var = |n: &str| self.env.vars.contains_key(n);
        // Built at runtime from a constant template (`fmt.Sprintf(coOccurrenceSQL, arms, …)`):
        // the template is read, each hole named by the functions that fill it.
        let (text, holes) = match text {
            Some(t) => (Some(t), vec![]),
            None => match c.args.get(n).and_then(|a| self.sql_template(a)) {
                Some((t, h)) => (Some(t), h),
                None => (None, vec![]),
            },
        };
        let fill = |s: String| -> String { holes.iter().enumerate().fold(s, |s, (i, h)| s.replace(&format!("__dyn{i}__"), h)) };
        let Some(mut flow) = text.as_deref().and_then(|q| crate::sqlflow::analyze_with(q, &value, &is_var)) else {
            self.e.unknowns.push(Unknown { question: format!("SQL of `{api}` is not a constant"), loc: c.loc.clone() });
            let key = EffectKey { kind: "db_query".into(), target: "<dynamic>".into(), method: None };
            self.e.effects.push(EffectFact { key, api: api.to_string(), payload: None, loc: c.loc.clone() });
            return;
        };
        // Text that says nothing SQL can read (not a statement at all) is not an effect to drop.
        let quiet = [
            "set", "reset", "analyze", "vacuum", "lock", "notify", "listen", "begin", "commit", "rollback", "grant", "revoke", "comment", "discard", "do",
            "refresh", "call",
        ];
        let first = text.as_deref().unwrap_or("").split_whitespace().next().unwrap_or("").to_lowercase();
        if flow.reads.is_empty() && flow.writes.is_empty() && flow.ddl.is_empty() && !quiet.contains(&first.as_str()) {
            self.e.unknowns.push(Unknown { question: format!("SQL of `{api}` could not be read"), loc: c.loc.clone() });
            let key = EffectKey { kind: "db_query".into(), target: "<dynamic>".into(), method: None };
            self.e.effects.push(EffectFact { key, api: api.to_string(), payload: None, loc: c.loc.clone() });
            return;
        }
        flow.filters = flow.filters.into_iter().map(&fill).collect();
        flow.columns = std::mem::take(&mut flow.columns).into_iter().map(|(k, v)| (k, fill(v))).collect();
        let mut push = |kind: &str, target: &str, method: Option<String>| {
            let key = EffectKey { kind: kind.into(), target: target.into(), method };
            self.e.effects.push(EffectFact { key, api: api.to_string(), payload: None, loc: c.loc.clone() });
        };
        for t in &flow.writes {
            push("db_write", t, None);
        }
        for t in flow.reads.difference(&flow.writes) {
            push("db_read", t, None);
        }
        for (verb, object) in &flow.ddl {
            push("db_schema", object, Some(verb.clone()));
        }
        let lines: Vec<String> = text.as_deref().unwrap_or("").lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()).collect();
        for f in flow.filters {
            let loc = Loc { line: c.loc.line + sql_line(&lines, &f) as u32, ..c.loc.clone() };
            self.e.filters.push(QueryFilter { expr: f, loc });
        }
        for (field, v) in flow.columns {
            // `'CANCELLED'` is the value CANCELLED.
            let v = match v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
                Some(inner) if !inner.contains('\'') => inner.to_string(),
                _ => v,
            };
            self.e.writes.push(WriteFact { field, value: Some(v), creation: false, loc: c.loc.clone() });
        }
    }

    /// An SQL string built from a constant template: the template with each hole as
    /// `__dynN__`, and what fills each (`⟨built by scopePredicate⟩`).
    fn sql_template(&self, e: &Expr) -> Option<(String, Vec<String>)> {
        let e = self.inline_consts(e);
        let mut holes = vec![];
        let hole = |x: &Expr, holes: &mut Vec<String>| -> String {
            if let Some(v) = self.ix.const_str(self.env, x) {
                return v;
            }
            let makers = self.makers(x);
            let what = match makers.iter().collect::<Vec<_>>().as_slice() {
                [] => "⟨dynamic⟩".to_string(),
                // One builder: what it composes (`⟨a⟩ AND ⟨b, each c in cs⟩`) when that can be
                // read, so a builder that now ANDs in another term shows the term.
                [m] => {
                    let shape = self.maker_id(m).and_then(|id| clause_shape(self.ix, self.envs, &id, 0));
                    let direct = matches!(x, Expr::Call(c) if matches!(self.ix.resolve_call(self.env, &c.callee), Callee::Internal(_)));
                    match (shape, self.joined_by(x).as_deref()) {
                        (Some(s), Some("OR")) => format!("any of ({s})"),
                        (Some(s), Some("AND")) => format!("all of ({s})"),
                        (None, Some("OR")) => format!("any of (⟨built by {m}⟩)"),
                        (None, Some("AND")) => format!("all of (⟨built by {m}⟩)"),
                        (Some(s), _) if direct => s,
                        // Built into something else first (`WHEN i THEN (…)` arms): say by what.
                        (Some(s), _) => format!("⟨built by {m}: {s}⟩"),
                        (None, _) => format!("⟨built by {m}⟩"),
                    }
                }
                ms => format!("⟨built by {}⟩", ms.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ")),
            };
            holes.push(what);
            format!("__dyn{}__", holes.len() - 1)
        };
        let text = match &e {
            Expr::Call(call) if matches!(self.ix.resolve_call(self.env, &call.callee), Callee::External(ref a) if a == "fmt.Sprintf()") => {
                let format = self.ix.const_str(self.env, call.args.first()?)?;
                let mut out = String::new();
                let mut args = call.args.iter().skip(1);
                let mut chars = format.chars().peekable();
                while let Some(ch) = chars.next() {
                    if ch != '%' {
                        out.push(ch);
                        continue;
                    }
                    if chars.peek() == Some(&'%') {
                        chars.next();
                        out.push('%');
                        continue;
                    }
                    while chars.peek().is_some_and(|c| !c.is_ascii_alphabetic()) {
                        chars.next();
                    }
                    chars.next();
                    match args.next() {
                        Some(a) => out.push_str(&hole(a, &mut holes)),
                        None => out.push('?'),
                    }
                }
                out
            }
            Expr::Template { quasis, exprs } => {
                let mut out = String::new();
                for (i, q) in quasis.iter().enumerate() {
                    out.push_str(q);
                    if let Some(x) = exprs.get(i) {
                        out.push_str(&hole(x, &mut holes));
                    }
                }
                out
            }
            _ => return None,
        };
        Some((text, holes))
    }

    /// Loop filters of a loop body. `if c { continue }` skips when `c`; a trailing
    /// `if c { … }` skips when `¬c`, and the filters inside it are the loop's too, so
    /// `if (id) { …; if (v) { … } }` and `if (!id) continue; …; if (!v) continue; …` agree.
    fn loop_skips(&mut self, body: &[Stmt], over: &str) {
        for (k, s) in body.iter().enumerate() {
            let Stmt::If { test, then, otherwise, loc } = s else { continue };
            if !otherwise.is_empty() {
                continue;
            }
            // Only an `if` that does nothing but move on filters; one that does work first
            // (`if … { arms = append(…); continue }`) is another way of handling the item.
            if matches!(then.as_slice(), [Stmt::Jump(_)]) {
                self.e.skips.push(SkipFact { over: over.to_string(), when: self.pred(test).render(), loc: loc.clone() });
            } else if k + 1 == body.len() && !matches!(then.last(), Some(Stmt::Return(..) | Stmt::Throw(..))) {
                self.e.skips.push(SkipFact { over: over.to_string(), when: self.pred(test).negate().render(), loc: loc.clone() });
                self.loop_skips(then, over);
            }
        }
    }

    /// Program functions whose results `e` is made from: called in it, or flowing in through
    /// the locals it uses.
    /// The program function a maker name (from `makers`) refers to, preferring this module's.
    fn maker_id(&self, name: &str) -> Option<String> {
        let named = |f: &&ir::Function| f.id.rsplit("::").next().is_some_and(|l| l.rsplit('.').next() == Some(name));
        let mut fs: Vec<&ir::Function> = self.ix.program.functions().filter(named).collect();
        fs.sort_by_key(|f| !f.id.starts_with(&format!("{}::", self.env.module)));
        fs.first().map(|f| f.id.clone())
    }

    /// `strings.Join(terms, " OR ")` / `terms.join(" AND ")`: the connective the parts are joined by.
    fn joined_by(&self, e: &Expr) -> Option<String> {
        let Expr::Call(c) = e else { return None };
        let sep = match self.ix.resolve_call(self.env, &c.callee) {
            Callee::External(a) if a == "strings.Join()" => c.args.get(1),
            Callee::External(a) if a == "Array#.join()" => c.args.first(),
            _ => None,
        }?;
        let sep = self.ix.const_str(self.env, sep)?.trim().to_uppercase();
        matches!(sep.as_str(), "OR" | "AND").then_some(sep)
    }

    fn makers(&self, e: &Expr) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        visit_expr(e, &mut |call| {
            if let Callee::Internal(id) = self.ix.resolve_call(self.env, &call.callee) {
                out.insert(id.rsplit("::").next().unwrap_or(&id).rsplit('.').next().unwrap_or(&id).to_string());
            }
        });
        let mut idents = vec![];
        collect_idents(e, &mut idents);
        for n in idents {
            if let Some(m) = self.made_by.get(&n) {
                out.extend(m.iter().cloned());
            }
        }
        out
    }

    /// `e` with numeric module constants replaced by their values, so a formula reads
    /// `0.25 * math.Log1p(breadth)` and a changed weight is a changed formula.
    fn inline_consts(&self, e: &Expr) -> Expr {
        let b = |x: &Expr| Box::new(self.inline_consts(x));
        match e {
            Expr::Ident(n) if !self.env.vars.contains_key(n) => match self.ix.const_str(self.env, e).and_then(|v| v.parse::<f64>().ok()) {
                Some(v) => Expr::Num(v),
                None => e.clone(),
            },
            // A local by what it was built from (already substituted when it was built).
            Expr::Ident(n) if self.def_exprs.contains_key(n) && !matches!(self.def_exprs[n], Expr::Ident(_)) => self.def_exprs[n].clone(),
            Expr::Template { quasis, exprs } => Expr::Template { quasis: quasis.clone(), exprs: exprs.iter().map(|x| self.inline_consts(x)).collect() },
            Expr::Binary { op, left, right } => Expr::Binary { op: *op, left: b(left), right: b(right) },
            Expr::Logical { op, left, right } => Expr::Logical { op: *op, left: b(left), right: b(right) },
            Expr::Not(x) => Expr::Not(b(x)),
            Expr::Conditional { test, then, otherwise } => Expr::Conditional { test: b(test), then: b(then), otherwise: b(otherwise) },
            Expr::Call(c) => {
                Expr::Call(Box::new(ir::Call { callee: c.callee.clone(), args: c.args.iter().map(|a| self.inline_consts(a)).collect(), loc: c.loc.clone() }))
            }
            Expr::Array(xs) => Expr::Array(xs.iter().map(|x| self.inline_consts(x)).collect()),
            other => other.clone(),
        }
    }

    /// `kysely.Kysely#.selectFrom().where().execute()`: the last link only runs a chain whose
    /// root is a catalogued effect.
    fn runs_effect_chain(&self, api: &str) -> bool {
        let links: Vec<&str> = api.trim_end_matches("()").split("().").collect();
        let Some(last) = links.last() else { return false };
        // `…execute()` runs the chain; so does a catalogued effect at its end whose chain
        // already holds one (gorm's `Raw("SELECT …").Scan(&x)`).
        let runs = last.starts_with("execute") || *last == "stream" || self.cat.effect_for(api).is_some();
        if !runs || links.len() < 2 {
            return false;
        }
        (1..links.len()).any(|n| self.cat.effect_for(&format!("{}()", links[..n].join("()."))).is_some())
    }

    /// The fields an HTTP call sends: the `body` of its options (through `JSON.stringify`),
    /// or an object passed as the data argument (`axios.post(url, { … })`).
    fn request_body(&self, c: &ir::Call) -> Option<String> {
        let body = c.args.iter().skip(1).find_map(|a| match a {
            Expr::Object(props) => Some(props.iter().find(|(k, _)| k == "body" || k == "data").map(|(_, v)| v).unwrap_or(a)),
            _ => None,
        })?;
        let body = match body {
            Expr::Call(inner) if matches!(&inner.callee, Expr::Member { object, property } if property == "stringify" && matches!(object.as_ref(), Expr::Ident(n) if n == "JSON")) => {
                inner.args.first()?
            }
            other => other,
        };
        let Expr::Object(props) = body else { return None };
        // Fields in source order; a conditional spread `...(c ? { x } : {})` sends `x?`.
        let mut fields = vec![];
        for (k, v) in props {
            if k != "..." {
                fields.push(k.clone());
                continue;
            }
            match v {
                Expr::Conditional { then, otherwise, .. } => {
                    for side in [then, otherwise] {
                        if let Expr::Object(inner) = side.as_ref() {
                            fields.extend(inner.iter().map(|(k, _)| format!("{k}?")));
                        }
                    }
                }
                Expr::Logical { op: ir::LogicOp::And, right, .. } => {
                    if let Expr::Object(inner) = right.as_ref() {
                        fields.extend(inner.iter().map(|(k, _)| format!("{k}?")));
                    }
                }
                other => fields.push(format!("...{}", render(other))),
            }
        }
        Some(format!("{{{}}}", fields.join(", ")))
    }

    /// Resolve the handler argument of a subscription/route call to an entity id.
    fn handler_arg(&mut self, c: &ir::Call, idx: i32) -> Option<String> {
        let i = if idx < 0 { c.args.len() as i32 + idx } else { idx };
        let arg = c.args.get(usize::try_from(i).ok()?)?;
        // `http.HandlerFunc(h.cancel)` adapts the handler it is given.
        let arg = match arg {
            Expr::Call(c) if c.args.len() == 1 && matches!(c.args[0], Expr::Closure(_) | Expr::Member { .. } | Expr::Ident(_)) => &c.args[0],
            other => other,
        };
        let id = match arg {
            Expr::Closure(id) => id.clone(),
            other => match self.ix.resolve_call(self.env, other) {
                Callee::Internal(id) => id,
                _ => return None,
            },
        };
        self.consumed_closures.insert(id.clone());
        Some(id)
    }

    // ------------------------------------------------------------ query predicates

    /// Canonical text of a filter call: `col op value`, `(a ∨ b)`, `¬a`, `exists(subquery)`.
    fn filter_text(&self, env: &Env, c: &ir::Call) -> String {
        let method = match &c.callee {
            Expr::Member { property, .. } => property.as_str(),
            _ => "",
        };
        // `Where("status = ? AND total > ?", s, 0)` (gorm): the placeholders filled in.
        if let Some(q) = c.args.first().and_then(|a| self.ix.const_str(env, a)) {
            // A column name has no spaces; an SQL fragment does.
            if q.trim().contains(' ') {
                let info = crate::sql::parse(&format!("select * from t where {q}"), &|k| c.args.get(k).and_then(|a| self.ix.const_str(env, a)));
                if let Some(info) = info {
                    return info.filters.join(" ∧ ");
                }
            }
        }
        match (method, c.args.as_slice()) {
            ("and" | "or", [Expr::Array(items)]) => {
                let sep = if method == "and" { " ∧ " } else { " ∨ " };
                format!("({})", items.iter().map(|i| self.predicate(env, i)).collect::<Vec<_>>().join(sep))
            }
            ("not", [x]) => format!("¬{}", self.predicate(env, x)),
            ("exists", [x]) => format!("exists({})", self.query_text(env, x).unwrap_or_else(|| self.predicate(env, x))),
            ("and" | "or", [x]) => format!("{method}({})", self.predicate(env, x)),
            (_, [x]) => self.predicate(env, x),
            (_, [l, op, r]) => format!("{} {} {}", self.operand(env, l), self.operand(env, op), self.operand(env, r)),
            (_, args) => format!("{method}({})", args.iter().map(|x| self.operand(env, x)).collect::<Vec<_>>().join(", ")),
        }
    }

    /// A whole predicate: as an operand, but one built dynamically keeps its source
    /// text (`eb.and(predicates)`), so a reviewer sees what it was.
    fn predicate(&self, env: &Env, e: &Expr) -> String {
        if let Expr::Closure(id) = e {
            return match (returned(self.ix, id), self.envs.by_fn.get(id)) {
                (Some(r), Some(cenv)) => self.predicate(cenv, r),
                _ => "`<callback>`".into(),
            };
        }
        match self.operand(env, e) {
            v if v == "_" => format!("`{}`", render(e)),
            v => v,
        }
    }

    /// A filter operand: a constant, a nested predicate, what a callback returns,
    /// or a subquery. Anything else is `_`.
    fn operand(&self, env: &Env, e: &Expr) -> String {
        if let Some(v) = self.ix.const_str(env, e) {
            return v;
        }
        match e {
            Expr::Await(x) => self.operand(env, x),
            Expr::Array(xs) => format!("[{}]", xs.iter().map(|x| self.operand(env, x)).collect::<Vec<_>>().join(", ")),
            Expr::Closure(id) => match (returned(self.ix, id), self.envs.by_fn.get(id)) {
                (Some(r), Some(cenv)) => self.operand(cenv, r),
                _ => "_".into(),
            },
            // A predicate kept in a local: `const p = (eb) => eb(…); qb.where(p)`.
            Expr::Ident(n) => match env.vars.get(n) {
                Some(Ty::Function(id)) if self.ix.function(id).is_some_and(|f| f.parent.is_some()) => self.operand(env, &Expr::Closure(id.clone())),
                _ => "_".into(),
            },
            // A subquery first: its outermost link may itself be a `where`.
            Expr::Call(c) => match self.query_text(env, e) {
                Some(q) => format!("({q})"),
                None => match self.ix.resolve_call(env, &c.callee) {
                    Callee::External(api) if self.cat.is_filter(&api) => self.filter_text(env, c),
                    Callee::External(api) if self.cat.is_literal(&api) => c.args.first().map_or_else(|| "_".into(), |a| self.operand(env, a)),
                    _ => "_".into(),
                },
            },
            _ => "_".into(),
        }
    }

    /// A subquery chain as `table where f1 ∧ f2`, if `e` is one.
    fn query_text(&self, env: &Env, e: &Expr) -> Option<String> {
        let mut filters = vec![];
        let mut cur = e;
        loop {
            let Expr::Call(c) = cur else { return None };
            let Expr::Member { object, .. } = &c.callee else { return None };
            let Callee::External(api) = self.ix.resolve_call(env, &c.callee) else { return None };
            if self.cat.effect_for(&api).is_some_and(|r| r.kind == "db_read") {
                let table = c.args.first().map(|a| self.operand(env, a)).unwrap_or_else(|| "_".into());
                filters.reverse();
                return Some(if filters.is_empty() { table } else { format!("{table} where {}", filters.join(" ∧ ")) });
            }
            if self.cat.is_filter(&api) {
                filters.push(self.filter_text(env, c));
            }
            cur = object;
        }
    }

    // ------------------------------------------------------------ predicates

    fn subject(&self, e: &Expr) -> String {
        match e {
            Expr::Member { object, property } => self.ix.field_path(self.env, object, property).unwrap_or_else(|| render(e)),
            other => render(other),
        }
    }

    fn pred(&self, e: &Expr) -> Pred {
        match e {
            Expr::Binary { op, left, right } if op.is_comparison() => {
                let sym = match op {
                    ir::BinOp::Eq => "==",
                    ir::BinOp::NotEq => "!=",
                    ir::BinOp::Lt => "<",
                    ir::BinOp::LtEq => "<=",
                    ir::BinOp::Gt => ">",
                    ir::BinOp::GtEq => ">=",
                    _ => unreachable!(),
                };
                let (lc, rc) = (self.ix.const_str(self.env, left), self.ix.const_str(self.env, right));
                // Put the constant (if any) on the right: `0 < n` is `n > 0`.
                let (subject, sym, value) = match (lc, rc) {
                    (None, Some(v)) => (self.subject(left), sym, v),
                    (Some(v), None) => (self.subject(right), flip(sym), v),
                    _ => return Pred::compare(self.subject(left), sym, self.subject(right)),
                };
                let set = |v: &str| BTreeSet::from([v.to_string()]);
                // A size is never negative: `n > 0`, `n >= 1`, `n != 0` all mean `n ∉ {0}`.
                let is_size = subject.ends_with(".length") || subject.ends_with(".size");
                match (sym, value.as_str()) {
                    ("==", _) => Pred::In { field: subject, values: set(&value) },
                    ("!=", _) => Pred::NotIn { field: subject, values: set(&value) },
                    (">", "0") | (">=", "1") if is_size => Pred::NotIn { field: subject, values: set("0") },
                    ("<", "1") | ("<=", "0") if is_size => Pred::In { field: subject, values: set("0") },
                    _ => Pred::compare(subject, sym, value),
                }
            }
            Expr::Logical { op: ir::LogicOp::And, left, right } => Pred::And(vec![self.pred(left), self.pred(right)]).simplify(),
            Expr::Logical { op: ir::LogicOp::Or, left, right } => Pred::Or(vec![self.pred(left), self.pred(right)]).simplify(),
            Expr::Not(x) => self.pred(x).negate(),
            Expr::Await(x) => self.pred(x),
            Expr::Call(c) => {
                // `[A, B].includes(x)`
                if let Expr::Member { object, property } = &c.callee {
                    if let (Expr::Array(items), "includes", Some(arg)) = (object.as_ref(), property.as_str(), c.args.first()) {
                        let values: Option<BTreeSet<String>> = items.iter().map(|i| self.ix.const_str(self.env, i)).collect();
                        if let Some(values) = values {
                            return Pred::In { field: self.subject(arg), values };
                        }
                    }
                }
                match self.ix.resolve_call(self.env, &c.callee) {
                    Callee::Internal(entity) => Pred::Call { entity, negated: false },
                    // By canonical name, so `semver.satisfies(v)` and `satisfies(v)` agree.
                    Callee::External(canon) => {
                        let args = c.args.iter().map(render).collect::<Vec<_>>().join(", ");
                        Pred::Opaque { text: format!("{}({args})", canon.trim_end_matches("()")), negated: false }
                    }
                    Callee::Unresolved(_) => Pred::Opaque { text: render(e), negated: false },
                }
            }
            Expr::Member { .. } | Expr::Ident(_) => Pred::Truthy(self.subject(e)),
            Expr::Bool(true) => Pred::True,
            Expr::Bool(false) => Pred::False,
            other => Pred::Opaque { text: render(other), negated: false },
        }
    }

    fn with_domain(&self, p: Pred) -> Pred {
        let ix = self.ix;
        p.with_domain(&|field: &str| {
            let (rec, f) = field.split_once('.')?;
            ix.field_domain(rec, f)
        })
    }
}

struct Site<'w, 'a, 'p> {
    segments: Vec<String>,
    walker: &'w Walker<'a, 'p>,
    call: &'w ir::Call,
}

impl CallSite for Site<'_, '_, '_> {
    fn segments(&self) -> &[String] {
        &self.segments
    }

    fn arg_count(&self) -> usize {
        self.call.args.len()
    }

    fn arg_str(&self, i: usize) -> Option<String> {
        self.walker.ix.const_str(self.walker.env, self.call.args.get(i)?)
    }

    fn chain_model(&self) -> Option<String> {
        if let Some(t) = self.arg_type(0) {
            return Some(t);
        }
        let mut cur = &self.call.callee;
        while let Expr::Member { object, .. } = cur {
            let Expr::Call(c) = object.as_ref() else { return None };
            if let Expr::Member { property, .. } = &c.callee {
                let site = Site { segments: vec![], walker: self.walker, call: c };
                match property.as_str() {
                    "Model" => return site.arg_type(0),
                    "Table" => return site.arg_str(0),
                    _ => {}
                }
            }
            cur = &c.callee;
        }
        None
    }

    fn arg_type(&self, i: usize) -> Option<String> {
        let ty = self.walker.ix.type_of(self.walker.env, self.call.args.get(i)?);
        let ty = match ty {
            Ty::Array(el) => *el,
            t => t,
        };
        match ty {
            Ty::Class(id) => self.walker.ix.class(&id).map(|c| c.name.clone()),
            Ty::Record { name, .. } => Some(name),
            _ => None,
        }
    }

    fn arg_prop(&self, i: usize, key: &str) -> Option<String> {
        match self.call.args.get(i)? {
            Expr::Object(props) => {
                let (_, v) = props.iter().find(|(k, _)| k == key)?;
                self.walker.ix.const_str(self.walker.env, v)
            }
            _ => None,
        }
    }
}

/// What a closure returns last: the predicate of `(eb) => { …; return eb.and(ps); }`.
fn returned<'p>(ix: &Index<'p>, id: &str) -> Option<&'p Expr> {
    ix.function(id)?.body.iter().rev().find_map(|s| match s {
        Stmt::Return(Some(r), _) => Some(r),
        _ => None,
    })
}

/// Names of the common table expressions (`with('x', …)`) defined in a function or
/// the functions enclosing it: a CTE is visible from every callback of its query.
fn scope_ctes(ix: &Index, f: &ir::Function) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cur = Some(f);
    while let Some(g) = cur {
        visit_calls(&g.body, &mut |c| {
            if let (Expr::Member { property, .. }, Some(Expr::Str(name))) = (&c.callee, c.args.first()) {
                if property == "with" || property == "withRecursive" {
                    out.insert(name.split_whitespace().next().unwrap_or(name).to_string());
                }
            }
        });
        cur = g.parent.as_deref().and_then(|p| ix.function(p));
    }
    out
}

/// The line (from 0) of a statement's lines where the condition `scope: pred` is written:
/// after its CTE's definition when it has one, else the first line mentioning it.
fn sql_line(lines: &[String], filter: &str) -> usize {
    let (scope, pred) = filter.split_once(": ").unwrap_or(("", filter));
    let from = lines.iter().position(|l| l.starts_with(&format!("{scope} as")) || l.contains(&format!(" {scope} as ("))).unwrap_or(0);
    let head: String = pred.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
    let find = |needle: &str| lines[from..].iter().position(|l| l.contains(needle)).map(|p| p + from);
    // The whole condition when it is on one line, else where it starts.
    find(pred).or_else(|| find(&head)).or_else(|| find(head.split('(').next().unwrap_or(&head))).unwrap_or(0)
}

fn collect_idents(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Ident(n) => out.push(n.clone()),
        Expr::Member { object, .. } => collect_idents(object, out),
        Expr::Index { object, index } => {
            collect_idents(object, out);
            collect_idents(index, out);
        }
        Expr::Call(c) => {
            collect_idents(&c.callee, out);
            c.args.iter().for_each(|a| collect_idents(a, out));
        }
        Expr::New { args, .. } | Expr::Array(args) | Expr::Template { exprs: args, .. } => args.iter().for_each(|a| collect_idents(a, out)),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            collect_idents(left, out);
            collect_idents(right, out);
        }
        Expr::Not(x) | Expr::Await(x) | Expr::Spawn(x) => collect_idents(x, out),
        Expr::Conditional { test, then, otherwise } => {
            collect_idents(test, out);
            collect_idents(then, out);
            collect_idents(otherwise, out);
        }
        Expr::Object(props) => props.iter().for_each(|(_, v)| collect_idents(v, out)),
        _ => {}
    }
}

/// A computed value worth showing in place of the local it was stored in: arithmetic.
fn is_formula(d: &str) -> bool {
    [" + ", " - ", " * ", " / ", "; +", "; −", "${"].iter().any(|op| d.contains(op))
}

/// `/prefix/path`, with duplicate and trailing slashes removed.
fn join_route(prefix: &str, path: &str) -> String {
    let segs: Vec<&str> = prefix.split('/').chain(path.split('/')).filter(|s| !s.is_empty()).collect();
    format!("/{}", segs.join("/"))
}

/// The operator with its operands swapped: `a < b` ⇔ `b > a`.
fn flip(op: &str) -> &str {
    match op {
        "<" => ">",
        "<=" => ">=",
        ">" => "<",
        ">=" => "<=",
        other => other,
    }
}

/// What a string-building function returns when it succeeds, with the program functions
/// it calls as atoms: `scopePredicate` → `⟨anchorPredicate(sc.Anchor)⟩ AND
/// ⟨anchorPredicate(c), each c in sc.Context⟩`. Failure returns (`""`, `null`) are not
/// shapes. `None` unless every other return agrees: a function that picks one of several
/// constants (`switch e.Kind { … }`) is a leaf, named rather than spelled out.
fn clause_shape(ix: &Index, envs: &Envs, id: &str, depth: u32) -> Option<String> {
    struct Shaper<'a, 'p> {
        ix: &'a Index<'p>,
        envs: &'a Envs,
        depth: u32,
        env: &'a Env,
        vals: HashMap<String, String>,
        shapes: BTreeSet<String>,
        /// A string literal was joined in: this builds text, not a number.
        text: std::cell::Cell<bool>,
    }
    impl Shaper<'_, '_> {
        fn walk(&mut self, stmts: &[Stmt], each: Option<&(String, String)>) {
            for s in stmts {
                match s {
                    Stmt::Let { name, init: Some(e), .. } => {
                        let v = self.shape(e, each);
                        self.vals.insert(name.clone(), v);
                    }
                    Stmt::Expr(Expr::Assign { target, value, .. }, _) => {
                        if let Expr::Ident(x) = target.as_ref() {
                            let v = self.shape(value, each);
                            self.vals.insert(x.clone(), v);
                        }
                    }
                    Stmt::If { then, otherwise, .. } => {
                        self.walk(then, each);
                        self.walk(otherwise, each);
                    }
                    Stmt::Loop { binding, iter, body, .. } => {
                        let over = match (binding, iter) {
                            (Some(b), Some(it)) => Some((b.clone(), render(it))),
                            _ => each.cloned(),
                        };
                        self.walk(body, over.as_ref());
                    }
                    Stmt::Block(b) => self.walk(b, each),
                    Stmt::Try { body, handler, finalizer, .. } => {
                        self.walk(body, each);
                        self.walk(handler, each);
                        self.walk(finalizer, each);
                    }
                    Stmt::Return(Some(e), _) => {
                        let first = match e {
                            Expr::Array(xs) => xs.first(),
                            e => Some(e),
                        };
                        match first {
                            None | Some(Expr::Null | Expr::Undefined) => {}
                            Some(Expr::Str(s)) if s.is_empty() => {}
                            Some(x) => {
                                let v = self.shape(x, each);
                                self.shapes.insert(v);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        fn shape(&self, e: &Expr, each: Option<&(String, String)>) -> String {
            match e {
                Expr::Str(s) => {
                    self.text.set(true);
                    s.clone()
                }
                Expr::Binary { op: ir::BinOp::Add, left, right } => self.shape(left, each) + &self.shape(right, each),
                Expr::Template { quasis, exprs } => {
                    self.text.set(true);
                    let mut out = String::new();
                    for (i, q) in quasis.iter().enumerate() {
                        out.push_str(q);
                        if let Some(x) = exprs.get(i) {
                            out.push_str(&self.shape(x, each));
                        }
                    }
                    out
                }
                Expr::Ident(n) if self.vals.contains_key(n) => self.vals[n].clone(),
                Expr::Call(c) => {
                    let text = match self.ix.resolve_call(self.env, &c.callee) {
                        Callee::Internal(id) => {
                            let name = id.rsplit("::").next().unwrap_or(&id).rsplit('.').next().unwrap_or(&id).to_string();
                            format!("{name}({})", c.args.iter().map(render).collect::<Vec<_>>().join(", "))
                        }
                        _ => render(e),
                    };
                    let mut ids = vec![];
                    collect_idents(e, &mut ids);
                    // A builder that composes one shape itself is spelled out: `scoped(e)` that
                    // returns `predicate(e)`'s clause reads as that clause.
                    if let Callee::Internal(id) = self.ix.resolve_call(self.env, &c.callee) {
                        let looped = each.is_some_and(|(b, _)| ids.contains(b));
                        if let Some(inner) = (!looped && self.depth < 3).then(|| clause_shape(self.ix, self.envs, &id, self.depth + 1)).flatten() {
                            self.text.set(true);
                            return format!("({inner})");
                        }
                    }
                    match each {
                        Some((b, over)) if ids.contains(b) => format!("⟨{text}, each {b} in {over}⟩"),
                        _ => format!("⟨{text}⟩"),
                    }
                }
                other => format!("⟨{}⟩", render(other)),
            }
        }
    }
    let f = ix.function(id)?;
    let mut s = Shaper { ix, envs, depth, env: envs.by_fn.get(id)?, vals: HashMap::new(), shapes: BTreeSet::new(), text: std::cell::Cell::new(false) };
    s.walk(&f.body, None);
    if !s.text.get() {
        return None;
    }
    let [one] = std::mem::take(&mut s.shapes).into_iter().collect::<Vec<_>>().try_into().ok()?;
    let one = crate::clause::unparen(&one.split_whitespace().collect::<Vec<_>>().join(" "));
    // A long builder reads better by name than spelled out.
    (one.chars().count() <= 200).then_some(one)
}
