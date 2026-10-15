//! Go: the go-shop fixture (net/http handlers, a service behind a repository interface,
//! raw SQL) under typical edits, plus the other frameworks the catalog knows.

use merak_cli::source::Files;
use std::path::Path;

fn shop() -> Files {
    merak_cli::source::from_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/go-shop")).unwrap()
}

fn files(entries: &[(&str, &str)]) -> Files {
    entries.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect()
}

/// The shop with `old` replaced by `new` in `file`.
fn edit(file: &str, old: &str, new: &str) -> Files {
    let mut f = shop();
    let text = f.get_mut(file).unwrap();
    assert!(text.contains(old), "{old:?} not in {file}");
    *text = text.replace(old, new);
    f
}

fn kinds(after: &Files) -> Vec<String> {
    merak_cli::diff(&shop(), after).unwrap().ops.iter().map(|o| format!("{} {}", o.kind, o.subject.rsplit("::").next().unwrap_or(&o.subject))).collect()
}

#[test]
fn the_shop_model() {
    let m = merak_cli::model(&shop()).unwrap();
    let routes: Vec<String> = m.routes.iter().map(|r| format!("{} {}", r.key(), r.handler)).collect();
    assert_eq!(routes, ["POST /orders/{id}/cancel internal/httpapi::Handler.cancel", "POST /orders/{id}/pay internal/httpapi::Handler.pay"]);
    // Through the `Repository` interface to its one implementation, and into the SQL.
    let cancel = &m.summaries["internal/orders::Service.Cancel"];
    let effects: Vec<String> = cancel.effects.keys().map(|e| e.render()).collect();
    assert_eq!(effects, ["db_read orders", "db_write orders", "http POST payments.example.com"]);
    let get = &m.entities["internal/orders::PostgresRepository.Get"];
    let filters: Vec<&str> = get.filters.iter().map(|f| f.expr.as_str()).collect();
    assert_eq!(filters, ["orders: id = ?", "orders: deleted_at is null"]);
    let sm = &m.state_machines["Order.Status"];
    let edges: Vec<String> = sm.edges.iter().map(|e| format!("{:?} → {}", e.from, e.to)).collect();
    assert_eq!(edges, [r#"{"PENDING"} → CANCELLED"#, r#"{"PENDING"} → PAID"#]);
    // `return s.repo.Save(ctx, o)` still calls Save; `fmt.Errorf` without `%w` is a new error.
    let pay = &m.entities["internal/orders::Service.Pay"];
    assert!(pay.calls.iter().any(|c| c.target == "internal/orders::PostgresRepository.Save"));
    assert!(pay.throws);
    // Error checks are named after the call that set the error.
    let handler = &m.entities["internal/httpapi::Handler.cancel"];
    assert_eq!(handler.guards[0].requires.render(), "err(svc.Cancel) ∈ {null}");
}

#[test]
fn a_dropped_ownership_check_is_a_guard_removal() {
    let t = kinds(&edit("internal/orders/service.go", "\tif o.UserID != userID {\n\t\treturn ErrForbidden\n\t}\n", ""));
    assert!(t.contains(&"GUARD_REMOVED Service.Cancel".to_string()), "{t:?}");
}

#[test]
fn rewording_a_wrapped_error_is_not_a_behaviour_change() {
    let t = kinds(&edit("internal/orders/service.go", r#"fmt.Errorf("get order: %w", err)"#, r#"fmt.Errorf("load order %s: %w", id, err)"#));
    assert!(t.is_empty(), "{t:?}");
}

#[test]
fn dropping_a_where_conjunct_widens_the_query() {
    let t = kinds(&edit("internal/orders/repo.go", " AND deleted_at IS NULL", ""));
    assert_eq!(t, ["QUERY_FILTER_REMOVED PostgresRepository.Get"]);
}

#[test]
fn a_goroutine_makes_effects_async() {
    let t = kinds(&edit("internal/orders/service.go", "\ts.refund(ctx, o)\n", "\tgo s.refund(ctx, o)\n"));
    assert_eq!(t, ["EFFECT_MADE_ASYNC Service.Cancel"]);
}

#[test]
fn a_widened_status_check_adds_a_transition_source() {
    let t = kinds(&edit(
        "internal/orders/service.go",
        "if o.Status != StatusPending {\n\t\treturn ErrNotCancellable",
        "if o.Status != StatusPending && o.Status != StatusPaid {\n\t\treturn ErrNotCancellable",
    ));
    assert_eq!(t, ["STATE_TRANSITION_SOURCE_WIDENED Order.Status → CANCELLED"]);
}

#[test]
fn moving_a_method_to_another_file_of_the_package_changes_nothing() {
    let mut f = shop();
    let svc = f.get_mut("internal/orders/service.go").unwrap();
    let at = svc.find("func (s *Service) refund").unwrap();
    let refund = svc.split_off(at);
    f.insert(
        "internal/orders/refund.go".into(),
        format!("package orders\n\nimport (\n\t\"bytes\"\n\t\"context\"\n\t\"encoding/json\"\n\t\"net/http\"\n)\n\n{refund}"),
    );
    assert!(kinds(&f).is_empty());
}

#[test]
fn a_log_message_is_logging_only() {
    let t = kinds(&edit("internal/orders/service.go", r#""order cancelled""#, r#""order was cancelled""#));
    assert_eq!(t, ["LOGGING_CHANGED Service.Cancel"]);
}

#[test]
fn a_new_route_is_an_entrypoint() {
    let t = kinds(&edit(
        "internal/httpapi/handler.go",
        "\tmux.HandleFunc(\"POST /orders/{id}/pay\", h.pay)\n",
        "\tmux.HandleFunc(\"POST /orders/{id}/pay\", h.pay)\n\tmux.HandleFunc(\"DELETE /orders/{id}\", h.cancel)\n",
    ));
    assert_eq!(t, ["ENTRYPOINT_ADDED DELETE /orders/{id}"]);
}

#[test]
fn gin_chi_gorm_and_sqlx() {
    let src = r#"
package api

import (
	"github.com/gin-gonic/gin"
	"github.com/go-chi/chi/v5"
	"github.com/jmoiron/sqlx"
	"gorm.io/gorm"
)

type User struct {
	ID    string
	Email string
}

type API struct {
	db  *gorm.DB
	sdb *sqlx.DB
}

func (a *API) Register(r *gin.Engine, c chi.Router) {
	r.POST("/users", a.create)
	c.Get("/users/{id}", a.get)
}

func (a *API) create(ctx *gin.Context) {
	u := User{Email: ctx.PostForm("email")}
	a.db.Create(&u)
}

func (a *API) get(w http.ResponseWriter, r *http.Request) {
	var u User
	a.sdb.Get(&u, "SELECT * FROM users WHERE id = ? AND active", r.PathValue("id"))
	var n []User
	a.db.Where("email LIKE ?", "%@acme.com").Find(&n)
}
"#;
    let m = merak_cli::model(&files(&[("api/api.go", src)])).unwrap();
    let routes: Vec<String> = m.routes.iter().map(|r| r.key()).collect();
    assert_eq!(routes, ["POST /users", "GET /users/{id}"]);
    let effects = |id: &str| -> Vec<String> { m.entities[id].effects.iter().map(|e| e.key.render()).collect() };
    assert_eq!(effects("api::API.create"), ["db_write User"]);
    assert_eq!(effects("api::API.get"), ["db_read users", "db_read User"]);
    let filters: Vec<&str> = m.entities["api::API.get"].filters.iter().map(|f| f.expr.as_str()).collect();
    assert_eq!(filters, ["users: id = ?", "users: active", "email like %@acme.com"]);
}

#[test]
fn a_spawned_closure_runs_later() {
    let before = "package jobs\nimport \"net/http\"\nfunc Notify() {\n\thttp.Post(\"https://hooks.example.com/x\", \"application/json\", nil)\n}\n";
    let after = "package jobs\nimport \"net/http\"\nfunc Notify() {\n\tgo func() {\n\t\thttp.Post(\"https://hooks.example.com/x\", \"application/json\", nil)\n\t}()\n}\n";
    let t = merak_cli::diff(&files(&[("jobs/jobs.go", before)]), &files(&[("jobs/jobs.go", after)])).unwrap();
    let k: Vec<&str> = t.ops.iter().map(|o| o.kind.as_str()).collect();
    assert!(k.contains(&"EFFECT_MADE_ASYNC"), "{k:?}");
}

fn go_diff(before: &str, after: &str) -> Vec<String> {
    let t = merak_cli::diff(&files(&[("store/store.go", before)]), &files(&[("store/store.go", after)])).unwrap();
    t.ops.iter().map(|o| format!("{} {}", o.kind, o.after.clone().or(o.before.clone()).unwrap_or_default())).collect()
}

const STORE: &str = r#"
package store

import "database/sql"

type Storage struct{ db *sql.DB }

func (s *Storage) Remove(userID int64, titles []string) error {
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	var count int
	query := "SELECT count(*) FROM categories WHERE user_id = $1 AND title != ANY($2)"
	if err := tx.QueryRow(query, userID, titles).Scan(&count); err != nil {
		return err
	}
	query = "DELETE FROM categories WHERE user_id = $1 AND title = ANY($2)"
	_, err = tx.Exec(query, userID, titles)
	return err
}
"#;

#[test]
fn a_reused_query_variable_is_read_statement_by_statement() {
    let t = go_diff(STORE, &STORE.replace("title != ANY($2)", "title <> ALL($2)"));
    assert_eq!(t, ["QUERY_FILTER_CHANGED categories: title <> all (?)"]);
}

#[test]
fn a_changed_argument_to_an_effect_is_not_a_refactor() {
    let src =
        "package srv\nimport \"os\"\nfunc Listen(path string) error {\n\tif err := os.Chmod(path, 0666); err != nil {\n\t\treturn err\n\t}\n\treturn nil\n}\n";
    let t = go_diff(src, &src.replace("0666", "0660"));
    assert_eq!(t.len(), 1, "{t:?}");
    assert!(t[0].starts_with("UNCLASSIFIED_CHANGE os.Chmod(_, 660)"), "{t:?}");
}

#[test]
fn struct_tags_are_the_wire_contract() {
    let src = "package feed\ntype Entry struct {\n\tTitle    string `xml:\"title\"`\n\tLanguage string `xml:\"lang,attr\"`\n}\n";
    let t = go_diff(src, &src.replace("lang,attr", "http://www.w3.org/XML/1998/namespace lang,attr"));
    assert_eq!(t, ["SCHEMA_FIELD_CHANGED string `xml:\"http://www.w3.org/XML/1998/namespace lang,attr\"`"]);
}

#[test]
fn a_switch_case_handing_on_the_error_is_propagation() {
    let src = r#"
package store

import (
	"database/sql"
	"errors"
	"fmt"
)

type Storage struct{ db *sql.DB }
type Icon struct{ ID int64 }

func (s *Storage) Icon(id int64) (*Icon, error) {
	var icon Icon
	err := s.db.QueryRow("SELECT id FROM icons WHERE id = $1", id).Scan(&icon.ID)
	switch {
	case errors.Is(err, sql.ErrNoRows):
		return nil, nil
	case err != nil:
		return nil, fmt.Errorf("store: icon %d: %w", id, err)
	default:
		return &icon, nil
	}
}
"#;
    let m = merak_cli::model(&files(&[("store/store.go", src)])).unwrap();
    let outputs: Vec<String> = m.entities["store::Storage.Icon"].outputs.iter().map(|o| format!("{} → {}", o.when.join(" ∧ "), o.value)).collect();
    assert_eq!(outputs, ["errors.Is(err, sql.ErrNoRows) → null", "!(errors.Is(err, sql.ErrNoRows)) → icon"]);
}

#[test]
fn gorm_raw_sql_and_model_tables() {
    let src = r#"
package repo

import "gorm.io/gorm"

type Order struct{ ID string }
type Repo struct{ db *gorm.DB }

func (r *Repo) Open() []Order {
	var out []Order
	r.db.Raw("SELECT * FROM orders WHERE status = 'open' AND deleted_at IS NULL").Scan(&out)
	return out
}

func (r *Repo) Close(id string) {
	r.db.Model(&Order{}).Where("id = ?", id).Update("status", "closed")
	r.db.Table("audit").Where("order_id = ?", id).Delete(nil)
}
"#;
    let m = merak_cli::model(&files(&[("repo/repo.go", src)])).unwrap();
    let effects = |id: &str| -> Vec<String> { m.entities[id].effects.iter().map(|e| e.key.render()).collect() };
    assert_eq!(effects("repo::Repo.Open"), ["db_read orders"]);
    assert_eq!(effects("repo::Repo.Close"), ["db_write Order", "db_delete audit"]);
    let filters: Vec<&str> = m.entities["repo::Repo.Open"].filters.iter().map(|f| f.expr.as_str()).collect();
    assert_eq!(filters, ["orders: status = 'open'", "orders: deleted_at is null"]);
}

const REFRESH: &str = r#"
CREATE TABLE IF NOT EXISTS stock (item_id text, stack_number text);

CREATE OR REPLACE FUNCTION refresh_stock() RETURNS integer
LANGUAGE plpgsql
AS $fn$
DECLARE
    v_rows integer;
BEGIN
    TRUNCATE stock;
    INSERT INTO stock (item_id, stack_number)
    WITH latest AS (
        SELECT sku_id AS item_id, remarks AS stack_number
          FROM inventory_count
         WHERE deleted_at IS NULL
    )
    SELECT item_id, stack_number FROM latest;
    GET DIAGNOSTICS v_rows = ROW_COUNT;
    RETURN v_rows;
END;
$fn$;
"#;

#[test]
fn sql_files_lineage_filters_and_tables() {
    let after = REFRESH.replace(
        "    SELECT item_id, stack_number FROM latest;",
        "    , stacks AS (\n        SELECT bi.item_id, bi.stack_numbers AS stack_number FROM bill_items bi JOIN bills b ON b.bill_id = bi.bill_id\n         WHERE b.status <> 'void'\n    )\n    SELECT l.item_id, s.stack_number FROM latest l LEFT JOIN stacks s ON s.item_id = l.item_id;",
    ).replace("stack_number text);", "stack_number text, note text);");
    let t = merak_cli::diff(&files(&[("db/stock.sql", REFRESH)]), &files(&[("db/stock.sql", &after)])).unwrap();
    let ops: Vec<String> = t.ops.iter().map(|o| format!("{} {}", o.kind, o.after.clone().unwrap_or_default())).collect();
    assert!(ops.contains(&"SCHEMA_FIELD_ADDED text".to_string()), "{ops:#?}");
    assert!(ops.contains(&"QUERY_FILTER_ADDED latest: left join stacks s on s.item_id = l.item_id".to_string()), "{ops:#?}");
    assert!(ops.contains(&"QUERY_FILTER_ADDED stacks: b.status <> 'void'".to_string()), "{ops:#?}");
    assert!(ops.iter().any(|o| o.starts_with("EFFECT_ADDED") && o.contains("bill_items")), "{ops:#?}");
    let m = merak_cli::model(&files(&[("db/stock.sql", &after)])).unwrap();
    let f = &m.entities["db/stock.sql::refresh_stock"];
    let w = f.writes.iter().find(|w| w.field == "stock.stack_number").unwrap();
    assert_eq!(w.value.as_deref(), Some("bill_items.stack_numbers"));
    let filter = f.filters.iter().find(|q| q.expr == "stacks: b.status <> 'void'").unwrap();
    assert_eq!(filter.loc.line, 19);
}

const RANK: &str = r#"
package rank

import "math"

const (
	wResults = 1.0
	wPrefix  = 6.0
)

type cand struct {
	count  int
	prefix bool
}

func sortCands(cs []cand) float64 {
	score := func(c cand) float64 {
		s := wResults * math.Log1p(float64(c.count))
		if c.prefix {
			s += wPrefix
		}
		return s
	}
	return score(cs[0])
}
"#;

#[test]
fn a_changed_weight_is_a_changed_computation() {
    let after = RANK.replace("wResults = 1.0", "wResults = 0.25");
    let t = merak_cli::diff(&files(&[("rank/rank.go", RANK)]), &files(&[("rank/rank.go", &after)])).unwrap();
    let ops: Vec<String> =
        t.ops.iter().map(|o| format!("{} {} → {}", o.kind, o.before.clone().unwrap_or_default(), o.after.clone().unwrap_or_default())).collect();
    assert!(ops.contains(&"CONSTANTS_CHANGED wResults = 1 → wResults = 0.25".to_string()), "{ops:#?}");
    assert!(
        ops.contains(&"OUTPUT_CHANGED otherwise: 1 * math.Log1p(c.count); +6 when cand.prefix is set → otherwise: 0.25 * math.Log1p(c.count); +6 when cand.prefix is set".to_string()),
        "{ops:#?}"
    );
    assert!(!ops.iter().any(|o| o.starts_with("PURE_REFACTOR")));
}

const SCOPES: &str = r#"
package suggest

import (
	"fmt"
	"strings"

	"gorm.io/gorm"
)

const partnersSQL = `SELECT b.id FROM products p JOIN brands b ON b.id = p.brand_id WHERE p.deleted_at IS NULL AND (%s)`

type Repo struct{ db *gorm.DB }

func predicate(e string) (string, bool) {
	if e == "" {
		return "", false
	}
	return "p.brand_id = ?", true
}

func scoped(e, ctx string) (string, bool) {
	clause, ok := predicate(e)
	if !ok {
		return "", false
	}
	return clause, true
}

func (r *Repo) Partners(anchors []string) {
	var terms []string
	for _, a := range anchors {
		clause, ok := predicate(a)
		if !ok {
			continue
		}
		terms = append(terms, "("+clause+")")
	}
	sql := fmt.Sprintf(partnersSQL, strings.Join(terms, " OR "))
	var ids []string
	r.db.Raw(sql).Scan(&ids)
}
"#;

#[test]
fn a_query_built_from_a_template_names_what_fills_it() {
    let after = SCOPES
        .replace("clause, ok := predicate(a)", "clause, ok := scoped(a, \"\")")
        .replace("		if !ok {\n			continue\n		}\n		terms", "		if !ok {\n			continue\n		}\n		if len(a) > 40 {\n			continue\n		}\n		terms");
    let t = merak_cli::diff(&files(&[("suggest/s.go", SCOPES)]), &files(&[("suggest/s.go", &after)])).unwrap();
    let ops: Vec<String> =
        t.ops.iter().map(|o| format!("{} {} → {}", o.kind, o.before.clone().unwrap_or_default(), o.after.clone().unwrap_or_default())).collect();
    // `scoped` only wraps `predicate`: both build the same clause, so the filter did not change.
    assert!(!ops.iter().any(|o| o.starts_with("QUERY_FILTER")), "{ops:#?}");
    assert!(ops.contains(&"SKIP_ADDED  → a in anchors: skipped when 40 < len(a)".to_string()), "{ops:#?}");
    let m = merak_cli::model(&files(&[("suggest/s.go", SCOPES)])).unwrap();
    let effects: Vec<String> = m.entities["suggest::Repo.Partners"].effects.iter().map(|e| e.key.render()).collect();
    assert_eq!(effects, ["db_read brands", "db_read products"]);
}

const COMPOSED: &str = r#"
package suggest

import (
	"fmt"
	"strings"

	"gorm.io/gorm"
)

const partnersSQL = `SELECT b.id FROM products p JOIN brands b ON b.id = p.brand_id WHERE p.deleted_at IS NULL AND (%s)`

type Entity struct {
	Kind  string
	ID    string
}

type Scope struct {
	Anchor  Entity
	Context []Entity
}

type Repo struct{ db *gorm.DB }

func anchorPredicate(e Entity) (string, []any, bool) {
	switch e.Kind {
	case "brand":
		return "p.brand_id = ?", []any{e.ID}, true
	case "range":
		return "p.product_range = ?", []any{e.ID}, true
	default:
		return "", nil, false
	}
}

func scopePredicate(sc Scope) (string, []any, bool) {
	clause, args, ok := anchorPredicate(sc.Anchor)
	if !ok {
		return "", nil, false
	}
	for _, c := range sc.Context {
		cc, cargs, ok := anchorPredicate(c)
		if !ok {
			return "", nil, false
		}
		clause = "(" + clause + ") AND (" + cc + ")"
		args = append(args, cargs...)
	}
	return clause, args, true
}

func (r *Repo) Partners(scopes []Scope) {
	var terms []string
	for _, sc := range scopes {
		clause, _, ok := anchorPredicate(sc.Anchor)
		if !ok {
			continue
		}
		terms = append(terms, "("+clause+")")
	}
	sql := fmt.Sprintf(partnersSQL, strings.Join(terms, " OR "))
	var ids []string
	r.db.Raw(sql).Scan(&ids)
}
"#;

#[test]
fn a_builder_that_ands_in_another_term_narrows_the_filter() {
	let after = COMPOSED.replace("clause, _, ok := anchorPredicate(sc.Anchor)", "clause, _, ok := scopePredicate(sc)");
	let t = merak_cli::diff(&files(&[("suggest/s.go", COMPOSED)]), &files(&[("suggest/s.go", &after)])).unwrap();
	let op = t.ops.iter().find(|o| o.kind == "QUERY_FILTER_CHANGED").unwrap_or_else(|| panic!("{:#?}", t.ops));
	assert_eq!(op.before.as_deref(), Some("products: any of (⟨built by anchorPredicate⟩)"));
	assert_eq!(op.after.as_deref(), Some("products: any of (⟨anchorPredicate(sc.Anchor)⟩ AND ⟨anchorPredicate(c), each c in sc.Context⟩)"));
	assert_eq!(op.note.as_deref(), Some("the query now also requires ⟨anchorPredicate(c), each c in sc.Context⟩: it reaches fewer rows"));
	let (before, after) = (files(&[("suggest/s.go", COMPOSED)]), files(&[("suggest/s.go", &after)]));
	let view = merak_cli::render(&t, "contracts", "test", &before, &after);
	assert!(view.contains("**`Repo.Partners` reads fewer rows:** rows must now also match ⟨anchorPredicate(c), each c in sc.Context⟩."), "{view}");
	let m = merak_cli::model(&before).unwrap();
	let returns: Vec<&str> = m.entities["suggest::scopePredicate"].outputs.iter().map(|o| o.value.as_str()).collect();
	assert!(returns.contains(&"[⟨anchorPredicate(sc.Anchor)⟩ AND ⟨anchorPredicate(c), each c in sc.Context⟩, args, true]"), "{returns:?}");
}

/// Calls inside a spread argument (`f(g(x)...)`) are calls, and a slice keeps its bounds:
/// wrapping what is passed in a new filter is a change, and the filter says what it returns.
#[test]
fn spread_arguments_and_slices_are_seen() {
    let before = files(&[(
        "vocab.go",
        "package vocab\n\nfunc push(xs ...string) {}\n\nfunc Load(names map[string][]string, b string) {\n\tpush(names[b]...)\n}\n",
    )]);
    let after = files(&[(
        "vocab.go",
        "package vocab\n\nfunc push(xs ...string) {}\n\nfunc singular(w string) string {\n\tif len(w) > 3 {\n\t\treturn w[:len(w)-1]\n\t}\n\treturn w\n}\n\nfunc only(xs []string) []string {\n\tout := []string{}\n\tfor _, x := range xs {\n\t\tout = append(out, singular(x))\n\t}\n\treturn out\n}\n\nfunc Load(names map[string][]string, b string) {\n\tpush(only(names[b])...)\n}\n",
    )]);
    let m = merak_cli::model(&after).unwrap();
    let load = &m.entities[".::Load"];
    assert!(load.calls.iter().any(|c| c.target == ".::only"), "{:?}", load.calls);
    let outputs: Vec<&str> = m.entities[".::singular"].outputs.iter().map(|o| o.value.as_str()).collect();
    assert_eq!(outputs, ["w.slice(0, len(w) - 1)", "w"]);
    // The caller's unexplained change is the call into the new helper, not the helper's insides.
    let t = merak_cli::diff(&before, &after).unwrap();
    let un: Vec<&str> = t.ops.iter().filter(|o| o.kind == "UNCLASSIFIED_CHANGE").filter_map(|o| o.after.as_deref()).collect();
    assert_eq!(un, ["only(_)"]);
}

/// Writing into a map the caller passed is behaviour the caller sees: a new helper that does
/// only that still has a contract, its write under `post`.
#[test]
fn writes_into_a_parameter_are_behaviour() {
    let after = files(&[(
        "vocab.go",
        "package vocab\n\nfunc add(into map[string]struct{}, names ...string) {\n\tfor _, n := range names {\n\t\tinto[n] = struct{}{}\n\t}\n}\n\nfunc local() int {\n\tm := map[string]int{}\n\tm[\"a\"] = 1\n\treturn len(m)\n}\n",
    )]);
    let m = merak_cli::model(&after).unwrap();
    let writes: Vec<&str> = m.entities[".::add"].mutates.iter().map(|w| w.field.as_str()).collect();
    assert_eq!(writes, ["into[n]"]);
    // A map of its own is not the caller's.
    assert!(m.entities[".::local"].mutates.is_empty());
}

/// The integrative links of a change: what a function now passes through new code, what it
/// builds to hand that code, and a search loop read as "for some".
#[test]
fn what_is_passed_through_new_code_is_linked() {
    let before = files(&[(
        "vocab.go",
        "package vocab\n\nfunc push(xs ...string) {}\n\nfunc Load(names map[string][]string, b string, cats []string) {\n\tfor _, c := range cats {\n\t\tpush(c)\n\t}\n\tpush(names[b]...)\n}\n",
    )]);
    let after = files(&[(
        "vocab.go",
        "package vocab\n\nfunc push(xs ...string) {}\n\nfunc add(into map[string]struct{}, ns ...string) {\n\tfor _, n := range ns {\n\t\tinto[n] = struct{}{}\n\t}\n}\n\nfunc only(xs []string, dim map[string]struct{}) []string {\n\tout := []string{}\n\tfor _, x := range xs {\n\t\tkeep := true\n\t\tfor _, w := range []string{x} {\n\t\t\tif _, d := dim[w]; d {\n\t\t\t\tkeep = false\n\t\t\t\tbreak\n\t\t\t}\n\t\t}\n\t\tif keep {\n\t\t\tout = append(out, x)\n\t\t}\n\t}\n\treturn out\n}\n\nfunc Load(names map[string][]string, b string, cats []string) {\n\tseen := map[string]struct{}{}\n\tfor _, c := range cats {\n\t\tadd(seen, c)\n\t\tpush(c)\n\t}\n\tpush(only(names[b], seen)...)\n}\n",
    )]);
    let m = merak_cli::model(&after).unwrap();
    let skips: Vec<String> = m.entities[".::only"].skips.iter().map(|s| format!("{}: {}", s.over, s.when)).collect();
    assert_eq!(skips, ["x in xs: for some w in [x]: dim.has(w)"]);
    let t = merak_cli::diff(&before, &after).unwrap();
    let load = t.contracts.iter().find(|c| c.name == "Load").unwrap();
    let links: Vec<String> = load.clauses.iter().filter(|x| matches!(x.row.as_str(), "passes" | "fills")).map(|x| format!("{} {} ({})", x.row, x.after.clone().unwrap_or_default(), x.tag.clone().unwrap_or_default())).collect();
    assert_eq!(links, ["passes push(only(names[b], seen)) (now through only)", "fills seen ← add(c) (filled here)"]);
    // Linked, the new calls need no hand check.
    let text = merak_cli::render(&t, "behaviour", "test", &before, &after);
    assert!(!text.contains("? CALLS"), "{text}");
    assert!(text.contains("names[b] now goes through new `only` before push(…)"), "{text}");
}

/// Moving logic is not removing it: a query extracted into a helper keeps its filters, a loop
/// filter that moved into new code's query says so, and a subquery rewrite is one change.
#[test]
fn moved_filters_are_moves_not_removals() {
    let base = "package app\n\nimport \"gorm.io/gorm\"\n\ntype Payment struct{ ID string; Status string }\n\ntype Repo struct{ db *gorm.DB }\n\n";
    let before = files(&[(
        "app.go",
        &format!("{base}func (r *Repo) Active() []Payment {{\n\tvar ps []Payment\n\tr.db.Table(\"payments\").Where(\"status = 'active'\").Where(\"deleted_at IS NULL\").Find(&ps)\n\treturn ps\n}}\n\nfunc (r *Repo) All() []Payment {{\n\tvar ps []Payment\n\tr.db.Find(&ps)\n\treturn ps\n}}\n\nfunc Check(r *Repo) int {{\n\tn := 0\n\tfor _, p := range r.All() {{\n\t\tif p.Status != \"pending\" {{\n\t\t\tcontinue\n\t\t}}\n\t\tn++\n\t}}\n\treturn n\n}}\n"),
    )]);
    let after = files(&[(
        "app.go",
        &format!("{base}func (r *Repo) Active() []Payment {{\n\treturn r.load()\n}}\n\nfunc (r *Repo) load() []Payment {{\n\tvar ps []Payment\n\tr.db.Table(\"payments\").Where(\"status = 'active'\").Where(\"deleted_at IS NULL\").Find(&ps)\n\treturn ps\n}}\n\nfunc (r *Repo) All() []Payment {{\n\tvar ps []Payment\n\tr.db.Find(&ps)\n\treturn ps\n}}\n\nfunc (r *Repo) Pending() []Payment {{\n\tvar ps []Payment\n\tr.db.Where(\"status = 'pending'\").Find(&ps)\n\treturn ps\n}}\n\nfunc Check(r *Repo) int {{\n\tn := 0\n\tfor range r.Pending() {{\n\t\tn++\n\t}}\n\treturn n\n}}\n"),
    )]);
    let t = merak_cli::diff(&before, &after).unwrap();
    let kinds: Vec<String> = t.ops.iter().filter(|o| o.layer != merak_transition::Layer::Structural).map(|o| format!("{} {}", o.kind, o.subject.rsplit("::").next().unwrap_or(""))).collect();
    assert!(!kinds.iter().any(|k| k.starts_with("QUERY_FILTER_REMOVED")), "{kinds:?}");
    assert!(kinds.contains(&"SKIP_MOVED_TO_QUERY Check".to_string()), "{kinds:?}");
    assert!(!kinds.iter().any(|k| k.starts_with("SKIP_REMOVED")), "{kinds:?}");
    let text = merak_cli::render(&t, "behaviour", "test", &before, &after);
    // The extracted query changed nothing: `Active` is not reported at all.
    assert!(!text.contains("Repo.Active ·"), "{text}");
    assert!(text.contains("moved into the query of `Repo.Pending`"), "{text}");
    assert!(!text.contains("▲ FILTER") && !text.contains("· ▲ wider"), "{text}");
}

/// A column written `COALESCE(existing, new)` keeps what is there; writing the new value
/// plainly overwrites it. Dropping that guard is a wider write, as a dropped check is.
#[test]
fn a_set_only_when_empty_write_becoming_an_overwrite_is_wider() {
    let src = |value: &str, skip: &str| {
        format!(
            "package app\n\nimport (\n\t\"strings\"\n\t\"gorm.io/gorm\"\n)\n\ntype Variant struct{{ ID string; ZohoItemID *string }}\n\nfunc Save(db *gorm.DB, id string, zid string) {{\n\tupdates := map[string]any{{}}\n\tif strings.TrimSpace(zid) != \"\" {{\n\t\tupdates[\"zoho_item_id\"] = {value}\n\t}}\n\tdb.Model(&Variant{{}}).Where(\"id = ?\", id).Updates(updates)\n}}\n\nfunc Audit(prior Variant, fields map[string]any) []string {{\n\tvar out []string\n\tfor col := range fields {{\n{skip}\t\tout = append(out, col)\n\t}}\n\treturn out\n}}\n"
        )
    };
    let skip = "\t\tif col == \"zoho_item_id\" {\n\t\t\tif prior.ZohoItemID != nil {\n\t\t\t\tcontinue\n\t\t\t}\n\t\t}\n";
    let before = files(&[("app.go", &src(r#"gorm.Expr("COALESCE(NULLIF(BTRIM(variants.zoho_item_id), ''), ?)", zid)"#, skip))]);
    let after = files(&[("app.go", &src("zid", ""))]);
    let t = merak_cli::diff(&before, &after).unwrap();
    let text = merak_cli::render(&t, "behaviour", "test", &before, &after);
    assert!(text.contains("▲ WRITES    now overwrites an existing zoho_item_id (it was set only when empty)"), "{text}");
    assert!(text.contains("no longer skips an iteration when col = zoho_item_id"), "{text}");
}

/// Deleting rows is its own effect, with the rows it removes: a reviewer reads "now deletes
/// CartItem rows where cart_id = ?", not "now writes CartItem".
#[test]
fn a_new_delete_says_which_rows_it_removes() {
    let base = "package app\n\nimport \"gorm.io/gorm\"\n\ntype CartItem struct{ ID string; CartID string }\n\ntype Repo struct{ db *gorm.DB }\n\n";
    let before = files(&[("app.go", &format!("{base}func (r *Repo) Clear(cartID string) {{\n}}\n"))]);
    let after = files(&[("app.go", &format!("{base}func (r *Repo) Clear(cartID string) {{\n\tr.db.Where(\"cart_id = ?\", cartID).Delete(&CartItem{{}})\n}}\n"))]);
    let t = merak_cli::diff(&before, &after).unwrap();
    let text = merak_cli::render(&t, "behaviour", "test", &before, &after);
    assert!(text.contains("now deletes CartItem rows where cart_id = ?"), "{text}");
}

/// Moving an inline block into a helper that returns the values it changed
/// (`content, ct, key = u.normalize(content, ct, key)`) passes the same `key` on: no
/// phantom "now passes …" or calls copied from how `key` was first built.
#[test]
fn values_reassigned_from_an_extracted_helper_are_not_their_old_definition() {
    let head = "package up\n\nimport (\n\t\"fmt\"\n\t\"path/filepath\"\n\t\"strings\"\n\t\"time\"\n)\n\ntype U struct{ On bool }\n\nfunc (u *U) url(k string) string { return \"https://cdn/\" + k }\n\n";
    let before = files(&[(
        "up.go",
        &format!("{head}func (u *U) Put(folder, name string, ct string) string {{\n\tkey := fmt.Sprintf(\"%s/%d_%s\", folder, time.Now().Unix(), name)\n\tif u.On {{\n\t\tif ext := \".jpg\"; !strings.EqualFold(filepath.Ext(key), ext) {{\n\t\t\tkey = strings.TrimSuffix(key, filepath.Ext(key)) + ext\n\t\t}}\n\t\tct = \"image/jpeg\"\n\t}}\n\t_ = ct\n\treturn u.url(key)\n}}\n"),
    )]);
    let after = files(&[(
        "up.go",
        &format!("{head}func (u *U) Put(folder, name string, ct string) string {{\n\tkey := fmt.Sprintf(\"%s/%d_%s\", folder, time.Now().Unix(), name)\n\tct, key = u.normalize(ct, key)\n\t_ = ct\n\treturn u.url(key)\n}}\n\nfunc (u *U) normalize(ct, key string) (string, string) {{\n\tif !u.On {{\n\t\treturn ct, key\n\t}}\n\tif ext := \".jpg\"; !strings.EqualFold(filepath.Ext(key), ext) {{\n\t\tkey = strings.TrimSuffix(key, filepath.Ext(key)) + ext\n\t}}\n\treturn \"image/jpeg\", key\n}}\n"),
    )]);
    let t = merak_cli::diff(&before, &after).unwrap();
    let text = merak_cli::render(&t, "behaviour", "test", &before, &after);
    assert!(!text.contains("PASSES"), "{text}");
    assert!(!text.contains("returns something else"), "{text}");
    assert!(!text.contains("NewString") && !text.contains("time.Now() when"), "{text}");
}

/// A condition on a method of a value names the value: `searches.through.Equal(started)`, not
/// the method's type (`time.Time#.Equal(started)`).
#[test]
fn a_method_condition_keeps_its_receiver() {
    let src = "package app\n\nimport \"time\"\n\ntype log struct{ through time.Time }\n\ntype S struct{ searches *log }\n\nfunc (s *S) Refresh(started time.Time) int {\n\tif !s.searches.through.Equal(started) {\n\t\treturn 0\n\t}\n\treturn 1\n}\n";
    let m = merak_cli::model(&files(&[("app.go", src)])).unwrap();
    let g: Vec<String> = m.entities[".::S.Refresh"].guards.iter().map(|g| g.requires.render()).collect();
    assert!(g.iter().any(|t| t.contains("searches.through.Equal(started)") && !t.contains('#')), "{g:?}");
}
