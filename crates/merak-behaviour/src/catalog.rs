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
}

#[derive(Debug, Clone, Deserialize)]
pub struct EffectRule {
    pub api: String,
    pub kind: String,
    pub target: String,
    #[serde(default)]
    pub method: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeRule {
    pub api: String,
    pub event: String,
    pub handler_arg: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteRule {
    pub api: String,
    pub method: String,
    pub path: String,
    pub handler_arg: i32,
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
        toml::from_str(DEFAULT).expect("built-in catalog.toml is valid")
    }

    /// Default catalog plus project rules (same schema) from `merak.toml`'s `[catalog]`.
    pub fn with_extensions(extra: Option<Catalog>) -> Catalog {
        let mut c = Catalog::default_catalog();
        if let Some(e) = extra {
            c.effect.extend(e.effect);
            c.subscribe.extend(e.subscribe);
            c.route.extend(e.route);
        }
        c
    }

    pub fn effect_for(&self, api: &str) -> Option<&EffectRule> {
        self.effect.iter().find(|r| api_matches(&r.api, api))
    }

    pub fn subscription_for(&self, api: &str) -> Option<&SubscribeRule> {
        self.subscribe.iter().find(|r| api_matches(&r.api, api))
    }

    pub fn route_for(&self, api: &str) -> Option<&RouteRule> {
        self.route.iter().find(|r| api_matches(&r.api, api))
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
    let segs = split_segments(api);
    expand_braces(pattern).iter().any(|p| {
        let ps = split_segments(p);
        ps.len() == segs.len() && ps.iter().zip(&segs).all(|(p, s)| segment_matches(p, s))
    })
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
        assert!(c.route_for("express.Router().post()").is_some());
        assert!(c.subscription_for("node:events.EventEmitter#.on()").is_some());
    }

    #[test]
    fn url_hosts() {
        assert_eq!(url_host("https://payments.example.com/refunds"), "payments.example.com");
    }
}
