//! Effect catalog: maps canonical external API names to behavioural effects,
//! event subscriptions and HTTP entry points. See `catalog.toml`.

use serde::Deserialize;

const DEFAULT: &str = include_str!("catalog.toml");

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub effect: Vec<EffectRule>,
    #[serde(default)]
    pub subscribe: Vec<SubscribeRule>,
    #[serde(default)]
    pub route: Vec<RouteRule>,
    #[serde(default)]
    pub filter: Vec<FilterRule>,
    #[serde(default)]
    pub callback: Vec<CallbackRule>,
    #[serde(default)]
    pub literal: Vec<FilterRule>,
    #[serde(default)]
    pub log: Vec<FilterRule>,
    #[serde(default)]
    pub access: AccessKeys,
    #[serde(default)]
    pub schema: SchemaKeys,
}

/// Validation-schema libraries and their documentation-only methods.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SchemaKeys {
    /// Canonical names of schema roots (`zod` for `import { z } from 'zod'`).
    #[serde(default)]
    pub roots: Vec<String>,
    /// Methods that document a schema without changing what it accepts.
    #[serde(default)]
    pub docs: Vec<String>,
}

/// Object-argument keys that state access requirements.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccessKeys {
    #[serde(default)]
    pub keys: Vec<String>,
    /// Keys whose value grants access (`public: true`) rather than requiring it.
    #[serde(default)]
    pub grants: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EffectRule {
    pub api: String,
    #[serde(skip)]
    pub pattern: ApiPattern,
    pub kind: String,
    pub target: String,
    #[serde(default)]
    pub method: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeRule {
    pub api: String,
    #[serde(skip)]
    pub pattern: ApiPattern,
    pub event: String,
    /// Argument holding the handler; omitted for decorators (the decorated method is the handler).
    #[serde(default)]
    pub handler_arg: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteRule {
    pub api: String,
    #[serde(skip)]
    pub pattern: ApiPattern,
    pub method: String,
    pub path: String,
    /// Argument holding the handler; omitted for decorators (the decorated method is the handler).
    #[serde(default)]
    pub handler_arg: Option<i32>,
    /// For decorator routes: the class decorator whose argument 0 prefixes `path`.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// A call matched by API name only: a query filter method (`where`, join `on`, …),
/// a literal wrapper (`sql.lit(…)`) or a log call.
#[derive(Debug, Clone, Deserialize)]
pub struct FilterRule {
    pub api: String,
    #[serde(skip)]
    pub pattern: ApiPattern,
}

/// The type of an untyped callback parameter passed to an external method, e.g.
/// the `qb` in `query.$if(cond, (qb) => qb.where(…))`.
#[derive(Debug, Clone, Deserialize)]
pub struct CallbackRule {
    pub api: String,
    #[serde(skip)]
    pub pattern: ApiPattern,
    /// Only closures passed as this argument; any argument when omitted.
    #[serde(default)]
    pub arg: Option<usize>,
    /// Canonical type of the first parameter, or `receiver`: the object the method is called on.
    pub param: String,
}

/// What a value spec can ask about a call site.
pub trait CallSite {
    fn segments(&self) -> &[String];
    fn arg_count(&self) -> usize;
    fn arg_str(&self, i: usize) -> Option<String>;
    fn arg_prop(&self, i: usize, key: &str) -> Option<String>;
}

impl Catalog {
    pub fn default_catalog() -> Catalog {
        let mut c: Catalog = toml::from_str(DEFAULT).expect("built-in catalog.toml is valid");
        c.compile();
        c
    }

    /// Default catalog plus project rules (same schema) from `merak.toml`'s `[catalog]`.
    pub fn with_extensions(extra: Option<Catalog>) -> Catalog {
        let mut c = Catalog::default_catalog();
        if let Some(e) = extra {
            c.effect.extend(e.effect);
            c.subscribe.extend(e.subscribe);
            c.route.extend(e.route);
            c.filter.extend(e.filter);
            c.callback.extend(e.callback);
            c.literal.extend(e.literal);
            c.log.extend(e.log);
            c.access.keys.extend(e.access.keys);
            c.access.grants.extend(e.access.grants);
            c.schema.roots.extend(e.schema.roots);
            c.schema.docs.extend(e.schema.docs);
        }
        c.compile();
        c
    }

    fn compile(&mut self) {
        self.effect.iter_mut().for_each(|r| r.pattern = ApiPattern::new(&r.api));
        self.subscribe.iter_mut().for_each(|r| r.pattern = ApiPattern::new(&r.api));
        self.route.iter_mut().for_each(|r| r.pattern = ApiPattern::new(&r.api));
        self.filter.iter_mut().for_each(|r| r.pattern = ApiPattern::new(&r.api));
        self.callback.iter_mut().for_each(|r| r.pattern = ApiPattern::new(&r.api));
        self.literal.iter_mut().for_each(|r| r.pattern = ApiPattern::new(&r.api));
        self.log.iter_mut().for_each(|r| r.pattern = ApiPattern::new(&r.api));
    }

    pub fn effect_for(&self, api: &str) -> Option<&EffectRule> {
        let segs: Vec<&str> = api.split('.').collect();
        self.effect.iter().find(|r| r.pattern.matches_segments(&segs))
    }

    /// Call-form subscription (`emitter.on('x', handler)`), or decorator form when `decorator`.
    pub fn subscription_for(&self, api: &str, decorator: bool) -> Option<&SubscribeRule> {
        self.subscribe.iter().find(|r| r.handler_arg.is_none() == decorator && r.pattern.matches(api))
    }

    /// Call-form route (`router.get('/x', handler)`), or decorator form when `decorator`.
    pub fn route_for(&self, api: &str, decorator: bool) -> Option<&RouteRule> {
        self.route.iter().find(|r| r.handler_arg.is_none() == decorator && r.pattern.matches(api))
    }

    pub fn is_filter(&self, api: &str) -> bool {
        self.filter.iter().any(|r| r.pattern.matches(api))
    }

    pub fn is_log(&self, api: &str) -> bool {
        self.log.iter().any(|r| r.pattern.matches(api))
    }

    pub fn is_access_key(&self, key: &str) -> bool {
        self.access.keys.iter().any(|k| k == key)
    }

    /// A call whose value is the constant in its first argument.
    pub fn is_literal(&self, api: &str) -> bool {
        self.literal.iter().any(|r| r.pattern.matches(api))
    }

    /// The rule typing a closure passed as argument `arg` of `api`.
    pub fn callback_for(&self, api: &str, arg: usize) -> Option<&CallbackRule> {
        self.callback.iter().find(|r| r.arg.is_none_or(|a| a == arg) && r.pattern.matches(api))
    }
}

pub fn split_segments(api: &str) -> Vec<String> {
    // Dots inside `(...)` never occur in canonical names, so a plain split is enough.
    api.split('.').map(str::to_string).collect()
}

/// Expand `{a,b}` groups (possibly several) into all alternatives.
pub fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(start) = pattern.find('{') else { return vec![pattern.to_string()] };
    let Some(len) = pattern[start..].find('}') else { return vec![pattern.to_string()] };
    let end = start + len;
    let (head, alts, tail) = (&pattern[..start], &pattern[start + 1..end], &pattern[end + 1..]);
    alts.split(',').flat_map(|alt| expand_braces(&format!("{head}{alt}{tail}"))).collect()
}

pub fn api_matches(pattern: &str, api: &str) -> bool {
    ApiPattern::new(pattern).matches(api)
}

/// A catalog `api` pattern with its `{a,b}` groups expanded and split into segments.
#[derive(Debug, Clone, Default)]
pub struct ApiPattern {
    alternatives: Vec<Vec<String>>,
}

impl ApiPattern {
    pub fn new(pattern: &str) -> Self {
        ApiPattern { alternatives: expand_braces(pattern).iter().map(|p| split_segments(p)).collect() }
    }

    pub fn matches(&self, api: &str) -> bool {
        self.matches_segments(&api.split('.').collect::<Vec<_>>())
    }

    fn matches_segments(&self, segs: &[&str]) -> bool {
        self.alternatives.iter().any(|ps| segments_match(ps, segs))
    }
}

/// `**` matches any number of segments (including none).
fn segments_match(ps: &[String], segs: &[&str]) -> bool {
    match ps.split_first() {
        None => segs.is_empty(),
        Some((p, rest)) if p == "**" => (0..=segs.len()).any(|k| segments_match(rest, &segs[k..])),
        Some((p, rest)) => segs.first().is_some_and(|s| segment_matches(p, s)) && segments_match(rest, &segs[1..]),
    }
}

fn segment_matches(p: &str, s: &str) -> bool {
    if p == s {
        return true;
    }
    // `*` matches one segment; `*#` / `*()` keep their suffix.
    match p.strip_prefix('*') {
        Some(suffix) => s.ends_with(suffix) && s.len() > suffix.len(),
        None => false,
    }
}

/// Evaluate a value spec against a call site.
pub fn eval_spec(spec: &str, site: &dyn CallSite) -> Option<String> {
    let (spec, upper) = match spec.strip_suffix(":upper") {
        Some(s) => (s, true),
        None => (spec, false),
    };
    let (spec, default) = match spec.split_once('|') {
        Some((s, d)) => (s, Some(d.to_string())),
        None => (spec, None),
    };
    let value = if let Some(n) = spec.strip_prefix("seg:") {
        let n: i64 = n.parse().ok()?;
        let segs = site.segments();
        let idx = if n < 0 { segs.len() as i64 + n } else { n };
        segs.get(idx as usize).map(|s| s.trim_end_matches("()").trim_end_matches('#').to_string())
    } else if let Some(n) = spec.strip_prefix("arg:") {
        site.arg_str(n.parse().ok()?)
    } else if let Some(n) = spec.strip_prefix("host:") {
        site.arg_str(n.parse().ok()?).map(|url| url_host(&url))
    } else if let Some(rest) = spec.strip_prefix("opt:") {
        let (n, key) = rest.split_once('.')?;
        site.arg_prop(n.parse().ok()?, key)
    } else {
        spec.strip_prefix("lit:").map(str::to_string)
    };
    value.or(default).map(|v| if upper { v.to_uppercase() } else { v })
}

pub fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_catalog_parses_and_matches() {
        let c = Catalog::default_catalog();
        assert_eq!(c.effect_for("@prisma/client.PrismaClient#.order.update()").unwrap().kind, "db_write");
        assert_eq!(c.effect_for("axios.post()").unwrap().kind, "http");
        assert_eq!(c.effect_for("fetch()").unwrap().kind, "http");
        assert!(c.effect_for("JSON.stringify()").is_none());
        assert!(c.route_for("express.Router().post()", false).is_some());
        assert!(c.route_for("@nestjs/common.Post()", true).is_some());
        assert!(c.route_for("@nestjs/common.Post()", false).is_none());
        assert!(c.subscription_for("node:events.EventEmitter#.on()", false).is_some());
        assert!(c.subscription_for("@nestjs/event-emitter.OnEvent()", true).is_some());
        assert!(c.is_filter("kysely.Kysely#.selectFrom().where()"));
        assert!(c.is_log("console.log()"));
        assert!(c.is_log("@nestjs/common.Logger#.warn()"));
        assert!(c.is_log("src/repositories/logging.repository.ts::LoggingRepository.warn()"));
        assert!(!c.is_log("src/order.service.ts::OrderService.cancel()"));
        assert!(c.is_filter("kysely.ExpressionBuilder#()"));
        assert!(!c.is_filter("kysely.Kysely#.selectFrom().select()"));
        assert_eq!(c.callback_for("kysely.Kysely#.selectFrom().$if()", 1).unwrap().param, "receiver");
        assert_eq!(c.effect_for("kysely.ExpressionBuilder#.selectFrom()").unwrap().kind, "db_read");
    }

    #[test]
    fn double_star_matches_any_depth() {
        let p = ApiPattern::new("kysely.**.where()");
        assert!(p.matches("kysely.Kysely#.selectFrom().where()"));
        assert!(p.matches("kysely.where()"));
        assert!(!p.matches("kysely.Kysely#.selectFrom()"));
    }

    #[test]
    fn url_hosts() {
        assert_eq!(url_host("https://payments.example.com/refunds"), "payments.example.com");
    }
}
