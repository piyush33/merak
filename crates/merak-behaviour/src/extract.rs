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
        Expr::Not(x) | Expr::Await(x) => visit_expr(x, f),
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
        ctes: scope_ctes(ix, f),
        path: vec![],
        consumed_closures: BTreeSet::new(),
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
        if let [(e, l)] = returns.as_slice() {
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
    /// CTE names in scope: reading one is not a table read.
    ctes: BTreeSet<String>,
    /// Conditions under which the current statement runs: enclosing `if` tests and the
    /// negations of earlier early exits. Part of every call shape, so a condition added
    /// around a call is a change, while `if (c) { f() }` and `if (!c) return; f()` agree.
    /// Top-level guards are marked (`true`): guard facts already compare them.
    path: Vec<(Pred, bool)>,
    consumed_closures: BTreeSet<String>,
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
            Stmt::Let { ty, init, loc, .. } => {
                if let Some(init) = init {
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
                    self.e.outputs.push(Output { when: self.conditions(false), value: render(e), renders, loc: loc.clone() });
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
            Stmt::Loop { iter, body, loc, .. } => {
                if let Some(it) = iter {
                    self.expr(it, loc);
                }
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
                // `new X(…)` is a call too: `await new Promise((r) => setTimeout(r, 1000))`.
                let call = ir::Call { callee: callee.as_ref().clone(), args: args.clone(), loc: nloc.clone() };
                let shape = format!("new {}", self.call_shape(&call));
                self.e.call_shapes.push(CallShape { shape, target: None, when: self.conditions(false), guards: self.conditions(true), loc: nloc.clone() });
                args.iter().for_each(|a| self.expr(a, nloc));
            }
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left, loc);
                self.expr(right, loc);
            }
            Expr::Not(x) | Expr::Await(x) => self.expr(x, loc),
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
                    self.e.calls.push(CallFact { target: id.clone(), loc: loc.clone(), conditional: true });
                    // By its name within this entity, so the conditions it is used under count.
                    let name = id.strip_prefix(&format!("{}.", self.e.id)).unwrap_or(id);
                    let shape = format!("<fn {name}>");
                    self.e.call_shapes.push(CallShape { shape, target: Some(id.clone()), when: self.conditions(false), guards: self.conditions(true), loc: loc.clone() });
                }
            _ => {}
        }
    }

    fn call(&mut self, c: &ir::Call) {
        let loc = &c.loc;
        self.access_args(&c.args, loc);
        let shape = self.call_shape(c);
        if self.api_name(&c.callee).is_some_and(|api| self.cat.is_log(&api)) {
            self.e.logs.push(CallShape { shape, target: None, when: self.conditions(false), guards: self.conditions(true), loc: loc.clone() });
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
                });
                self.e.calls.push(CallFact { target, loc: loc.clone(), conditional: self.depth > 0 });
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
            Callee::External(api) => {
                if !self.external_call(&api, c) {
                    self.e.call_shapes.push(CallShape { shape, target: None, when: self.conditions(false), guards: self.conditions(true), loc: loc.clone() });
                }
            }
            Callee::Unresolved(text) => {
                self.e.call_shapes.push(CallShape { shape, target: None, when: self.conditions(false), guards: self.conditions(true), loc: loc.clone() });
                self.e.unknowns.push(Unknown { question: format!("unresolved call `{text}`"), loc: loc.clone() });
            }
        }
        if let Expr::Member { object, .. } = &c.callee {
            self.expr(object, loc);
        }
        for a in &c.args {
            self.expr(a, loc);
        }
    }

    /// `target(args)`: the target by short name (stable across moves), constant
    /// arguments by value, object literals by key (and constant value), the rest `_`.
    fn call_shape(&self, c: &ir::Call) -> String {
        let target = match self.ix.resolve_call(self.env, &c.callee) {
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
        let mut conds: Vec<String> = self.path.iter().filter(|(_, g)| *g == guards).map(|(p, _)| p.render()).collect();
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
                self.e.effects.push(EffectFact { key, api: api.to_string(), loc: c.loc.clone() });
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

    /// Resolve the handler argument of a subscription/route call to an entity id.
    fn handler_arg(&mut self, c: &ir::Call, idx: i32) -> Option<String> {
        let i = if idx < 0 { c.args.len() as i32 + idx } else { idx };
        let arg = c.args.get(usize::try_from(i).ok()?)?;
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
