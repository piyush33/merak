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
