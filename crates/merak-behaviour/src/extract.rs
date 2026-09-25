//! Per-entity fact extraction: walks each lowered function body and records
//! calls, effects, reads, writes, guards, subscriptions and routes.

use crate::catalog::{self, CallSite, Catalog};
use crate::model::*;
use crate::pred::Pred;
use crate::resolve::{render, Callee, Env, Index, Ty};
use merak_ir::{self as ir, Expr, Loc, Stmt};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Build typing environments for every function (closures inherit their parent's).
pub fn build_envs(ix: &Index) -> HashMap<String, Env> {
    let mut envs = HashMap::new();
    let ids: Vec<String> = ix.program.functions().map(|f| f.id.clone()).collect();
    for id in ids {
        env_for(ix, &id, &mut envs);
    }
    envs
}

fn env_for(ix: &Index, id: &str, envs: &mut HashMap<String, Env>) -> Env {
    if let Some(e) = envs.get(id) {
        return e.clone();
    }
    let Some(f) = ix.function(id) else { return Env::default() };
    let module = id.split("::").next().unwrap_or("").to_string();
    let mut env = match &f.parent {
        Some(p) => env_for(ix, p, envs),
        None => Env { module: module.clone(), ..Default::default() },
    };
    if f.class.is_some() {
        env.this_class = Some(format!("{module}::{}", f.class.as_deref().unwrap_or("")));
    }
    for p in &f.params {
        let ty = p.ty.as_ref().map(|t| ix.resolve_type(&module, t)).unwrap_or(Ty::Unknown);
        env.vars.insert(p.name.clone(), ty);
    }
    collect_locals(ix, &f.body, &mut env);
    envs.insert(id.to_string(), env.clone());
    env
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

pub fn extract_entity(ix: &Index, cat: &Catalog, env: &Env, f: &ir::Function) -> Entity {
    let module = f.id.split("::").next().unwrap_or("").to_string();
    let mut w = Walker {
        ix,
        cat,
        env,
        depth: 0,
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
            throws: false,
            subscriptions: vec![],
            routes: vec![],
            unknowns: vec![],
        },
    };
    w.stmts(&f.body, true);
    if is_predicate_fn(f) {
        let returns: Vec<&Expr> = f
            .body
            .iter()
            .filter_map(|s| match s {
                Stmt::Return(Some(e), _) => Some(e),
                _ => None,
            })
            .collect();
        if returns.len() == 1 {
            w.e.returns = Some(w.pred(returns[0]));
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
    /// Nesting depth of conditional/loop/try regions.
    depth: u32,
    consumed_closures: BTreeSet<String>,
    e: Entity,
}

impl Walker<'_, '_> {
    fn stmts(&mut self, stmts: &[Stmt], top: bool) {
        for s in stmts {
            self.stmt(s, top);
        }
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
                        self.e.guards.push(GuardFact { requires, on_fail, loc: loc.clone() });
                    }
                }
                self.depth += 1;
                self.stmts(then, false);
                self.stmts(otherwise, false);
                self.depth -= 1;
            }
            Stmt::Return(e, loc) => {
                if let Some(e) = e {
                    self.expr(e, loc);
                }
            }
            Stmt::Throw(e, loc) => {
                self.e.throws = true;
                self.expr(e, loc);
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
            Expr::New { args, loc: nloc, .. } => args.iter().for_each(|a| self.expr(a, nloc)),
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
            Expr::Closure(id)
                // A closure used as a value (not registered as a handler/route) is
                // assumed to run on behalf of this entity.
                if !self.consumed_closures.contains(id) => {
                    self.e.calls.push(CallFact { target: id.clone(), loc: loc.clone(), conditional: true });
                }
            _ => {}
        }
    }

    fn call(&mut self, c: &ir::Call) {
        let loc = &c.loc;
        match self.ix.resolve_call(self.env, &c.callee) {
            Callee::Internal(target) => {
                self.e.calls.push(CallFact { target, loc: loc.clone(), conditional: self.depth > 0 });
            }
            Callee::External(api) => self.external_call(&api, c),
            Callee::Unresolved(text) => {
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

    fn external_call(&mut self, api: &str, c: &ir::Call) {
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
                let method = rule.method.as_ref().and_then(|m| catalog::eval_spec(m, &site));
                let dynamic = target.is_none();
                Found::Effect(EffectKey { kind: rule.kind.clone(), target: target.unwrap_or_else(|| "<dynamic>".into()), method }, dynamic)
            } else if let Some(rule) = self.cat.subscription_for(api) {
                Found::Subscribe(catalog::eval_spec(&rule.event, &site).unwrap_or_else(|| "<dynamic>".into()), rule.handler_arg)
            } else if let Some(rule) = self.cat.route_for(api) {
                Found::Route(
                    catalog::eval_spec(&rule.method, &site).unwrap_or_else(|| "?".into()),
                    catalog::eval_spec(&rule.path, &site).unwrap_or_else(|| "<dynamic>".into()),
                    rule.handler_arg,
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
            Found::Nothing => {}
        }
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

    // ------------------------------------------------------------ predicates

    fn subject(&self, e: &Expr) -> String {
        match e {
            Expr::Member { object, property } => self.ix.field_path(self.env, object, property).unwrap_or_else(|| render(e)),
            other => render(other),
        }
    }

    fn pred(&self, e: &Expr) -> Pred {
        match e {
            Expr::Binary { op: op @ (ir::BinOp::Eq | ir::BinOp::NotEq), left, right } => {
                let (subject, value) = match (self.ix.const_str(self.env, left), self.ix.const_str(self.env, right)) {
                    (None, Some(v)) => (self.subject(left), v),
                    (Some(v), None) => (self.subject(right), v),
                    _ => return Pred::Opaque { text: render(e), negated: false },
                };
                let values = BTreeSet::from([value]);
                if *op == ir::BinOp::Eq {
                    Pred::In { field: subject, values }
                } else {
                    Pred::NotIn { field: subject, values }
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
                    _ => Pred::Opaque { text: render(e), negated: false },
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
