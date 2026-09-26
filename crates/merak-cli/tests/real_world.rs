//! Patterns found running Merak on real repositories (immich's NestJS server),
//! each reduced to a minimal program.

use merak_cli::source::Files;

fn files(entries: &[(&str, &str)]) -> Files {
    entries.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect()
}

fn kinds(before: &Files, after: &Files) -> Vec<String> {
    merak_cli::diff(before, after).unwrap().ops.iter().map(|o| format!("{} {}", o.kind, o.subject)).collect()
}

const TSCONFIG: &str = r#"{
  // JSONC: comments and trailing commas
  "compilerOptions": { "paths": { "src/*": ["./src/*"], }, },
}"#;

const REPO: &str = r#"
import { Kysely } from 'kysely';
export class UserRepository {
  constructor(private db: Kysely<any>) {}
  update(id: string, dto: any) { return this.db.updateTable('user').set(dto).where('id', '=', id).execute(); }
}"#;

const BASE: &str = r#"
import { UserRepository } from 'src/repositories/user.repository.js';
export class BaseService {
  constructor(protected userRepository: UserRepository) {}
}"#;

fn app(service: &str) -> Files {
    files(&[
        ("server/tsconfig.json", TSCONFIG),
        ("server/src/repositories/user.repository.ts", REPO),
        ("server/src/services/base.service.ts", BASE),
        ("server/src/services/auth.service.ts", service),
    ])
}

#[test]
fn tsconfig_paths_and_inherited_injected_fields_resolve() {
    let service = r#"
import { BaseService } from 'src/services/base.service.js';
export class AuthService extends BaseService {
  change(id: string) { return this.userRepository.update(id, { password: 'x' }); }
}"#;
    let m = merak_cli::model(&app(service)).unwrap();
    let s = &m.summaries["server/src/services/auth.service.ts::AuthService.change"];
    let effects: Vec<String> = s.effects.keys().map(|e| e.render()).collect();
    assert_eq!(effects, ["db_write user"], "{:#?}", m.entities["server/src/services/auth.service.ts::AuthService.change"]);
}

#[test]
fn nestjs_decorators_declare_routes() {
    let controller = r#"
import { Controller, Get, Post } from '@nestjs/common';
@Controller('albums')
export class AlbumController {
  @Get() list() { return []; }
  @Post(':id/assets') add() { return 1; }
}"#;
    let m = merak_cli::model(&files(&[("src/album.controller.ts", controller)])).unwrap();
    let routes: Vec<String> = m.routes.iter().map(|r| r.key()).collect();
    assert_eq!(routes, ["GET /albums", "POST /albums/:id/assets"]);
}

#[test]
fn early_return_rewrite_is_not_a_guard_change() {
    let before = "export function f(paths: string[], a: number, b: number) { if (a > b) { return; } if (paths.length > 0) { console.log(paths); } }";
    let after = "export function f(paths: string[], a: number, b: number) { if (!(a <= b)) return; if (paths.length === 0) { return; } console.log(paths); }";
    let t = kinds(&files(&[("src/f.ts", before)]), &files(&[("src/f.ts", after)]));
    assert_eq!(t, ["PURE_REFACTOR *"]);
}

#[test]
fn import_style_does_not_change_guards() {
    let before = "import semver from 'semver'; export function f(v: string) { if (!semver.satisfies(v, '>=14')) throw new Error(); }";
    let after = "import { satisfies } from 'semver'; export function f(v: string) { if (!satisfies(v, '>=14')) throw new Error(); }";
    let t = kinds(&files(&[("src/f.ts", before)]), &files(&[("src/f.ts", after)]));
    assert_eq!(t, ["PURE_REFACTOR *"]);
}

#[test]
fn unmodelled_argument_change_is_unclassified_not_pure() {
    let service = |dto: &str| {
        format!(
            "import {{ BaseService }} from 'src/services/base.service.js';
export class AuthService extends BaseService {{
  change(id: string) {{ return this.userRepository.update(id, {dto}); }}
}}"
        )
    };
    let t = merak_cli::diff(&app(&service("{ password: 'x' }")), &app(&service("{ password: 'x', shouldChangePassword: false }"))).unwrap();
    assert_eq!(t.ops.len(), 1, "{:#?}", t.ops);
    let op = &t.ops[0];
    assert_eq!(op.kind, "UNCLASSIFIED_CHANGE");
    assert_eq!(op.after.as_deref(), Some(r#"UserRepository.update(_, {password="x", shouldChangePassword="false"})"#));
}

#[test]
fn test_files_are_not_analyzed() {
    let dir = std::env::temp_dir().join(format!("merak-tests-{}", std::process::id()));
    for p in ["src/a.ts", "src/a.spec.ts", "src/b.test.ts", "test/fixtures.ts", "src/__mocks__/m.ts"] {
        let f = dir.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "export const x = 1;").unwrap();
    }
    let loaded: Vec<String> = merak_cli::source::from_dir(&dir).unwrap().into_keys().collect();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(loaded, ["src/a.ts"]);
}

// ---- M5b: Kysely query predicates -------------------------------------------

fn person_repo(body: &str) -> Files {
    let src = format!(
        "import {{ Kysely }} from 'kysely';
export class PersonRepository {{
  constructor(private db: Kysely<any>) {{}}
  {body}
}}"
    );
    files(&[("src/person.repository.ts", &src)])
}

fn ops(before: &str, after: &str) -> Vec<merak_transition::Op> {
    merak_cli::diff(&person_repo(before), &person_repo(after)).unwrap().ops
}

fn show(ops: &[merak_transition::Op]) -> Vec<String> {
    ops.iter()
        .map(|o| {
            format!("{} {} | {} -> {}", o.kind, o.subject.rsplit("::").next().unwrap(), o.before.as_deref().unwrap_or(""), o.after.as_deref().unwrap_or(""))
                .trim_end()
                .replace("|  ->", "| ->")
        })
        .collect()
}

#[test]
fn removed_where_is_a_widened_query_not_unclassified() {
    let before = "get() { return this.db.selectFrom('person').where('person.deletedAt', 'is', null).where('person.ownerId', '=', 'x').execute(); }";
    let after = "get() { return this.db.selectFrom('person').where('person.ownerId', '=', 'x').execute(); }";
    assert_eq!(show(&ops(before, after)), ["QUERY_FILTER_REMOVED PersonRepository.get | person.deletedAt is null ->"]);
}

#[test]
fn filter_in_untyped_if_callback_is_typed() {
    let body = |col: &str| {
        format!("reassign(ownerId: string) {{ return this.db.updateTable('asset_face').$if(!!ownerId, (qb) => qb.where('{col}', '=', ownerId)).execute(); }}")
    };
    let t = ops(&body("asset_face.personGroupId"), &body("asset.ownerId"));
    assert_eq!(show(&t), ["QUERY_FILTER_CHANGED PersonRepository.reassign | asset_face.personGroupId = _ -> asset.ownerId = _"]);
}

#[test]
fn where_ref_and_join_on_are_filters() {
    let before = "get() { return this.db.selectFrom('asset_face').innerJoin('asset', (join) => join.onRef('asset.id', '=', 'asset_face.assetId')).execute(); }";
    let after = "get() { return this.db.selectFrom('asset_face').innerJoin('asset', (join) => join.onRef('asset.id', '=', 'asset_face.assetId').on('asset.visibility', '!=', 'hidden')).execute(); }";
    assert_eq!(show(&ops(before, after)), ["QUERY_FILTER_ADDED PersonRepository.get | -> asset.visibility != hidden"]);
}

#[test]
fn expression_builder_callback_renders_into_the_parent() {
    let body = |combinator: &str| {
        format!("get(id: string) {{ return this.db.selectFrom('asset').where((eb) => eb.{combinator}([eb('asset.ownerId', '=', id), eb('asset.deletedAt', 'is', null)])).execute(); }}")
    };
    let t = ops(&body("or"), &body("and"));
    assert_eq!(
        show(&t),
        ["QUERY_FILTER_CHANGED PersonRepository.get | (asset.ownerId = _ ∨ asset.deletedAt is null) -> (asset.ownerId = _ ∧ asset.deletedAt is null)"]
    );
}

#[test]
fn moving_a_filter_into_a_callback_is_not_a_change() {
    let before = "get(id: string) { return this.db.selectFrom('asset').where('asset.ownerId', '=', id).execute(); }";
    let after = "get(id: string) { return this.db.selectFrom('asset').where((eb) => eb('asset.ownerId', '=', id)).execute(); }";
    let t = ops(before, after);
    assert!(t.iter().all(|o| o.layer == merak_transition::Layer::Structural), "{:#?}", show(&t));
}

#[test]
fn subqueries_in_callbacks_are_reads() {
    let src = "get() { return this.db.selectFrom('album').where((eb) => eb.exists(eb.selectFrom('album_asset').whereRef('album_asset.albumId', '=', 'album.id'))).execute(); }";
    let m = merak_cli::model(&person_repo(src)).unwrap();
    let effects: Vec<String> = m.summaries["src/person.repository.ts::PersonRepository.get"].effects.keys().map(|e| e.render()).collect();
    assert_eq!(effects, ["db_read album", "db_read album_asset"]);
    let filters: Vec<&str> = m.entities["src/person.repository.ts::PersonRepository.get"].filters.iter().map(|f| f.expr.as_str()).collect();
    assert_eq!(filters, ["exists(album_asset where album_asset.albumId = album.id)"]);
}

#[test]
fn new_if_callback_filter_is_reported_on_the_method() {
    let before = "get(id?: string) { return this.db.selectFrom('memory').execute(); }";
    let after = "get(id?: string) { return this.db.selectFrom('memory').$if(id !== undefined, (qb) => qb.where('id', '=', id!)).execute(); }";
    let behaviour: Vec<_> = ops(before, after).into_iter().filter(|o| o.layer != merak_transition::Layer::Structural).collect();
    assert_eq!(show(&behaviour), ["QUERY_FILTER_ADDED PersonRepository.get | -> id = _"]);
}

#[test]
fn subquery_operand_and_local_predicate() {
    let before = "get(ownerId: string) { return this.db.updateTable('asset_face').where('asset_face.personGroupId', 'in', (eb) => eb.selectFrom('person').select('person.id').where('person.ownerId', '=', ownerId)).execute(); }";
    let after = "get(ownerId: string) { const owned = (eb: FaceEb) => eb('asset.ownerId', '=', ownerId); return this.db.updateTable('asset_face').where(owned).execute(); }";
    let t = merak_cli::diff(
        &person_repo(before),
        &person_repo(after)
            .into_iter()
            .map(|(k, v)| (k, format!("import {{ ExpressionBuilder }} from 'kysely'; type FaceEb = ExpressionBuilder<any, 'asset'>;\n{v}")))
            .collect(),
    )
    .unwrap();
    assert_eq!(
        show(&t.ops).into_iter().filter(|o| o.starts_with("QUERY")).collect::<Vec<_>>(),
        ["QUERY_FILTER_CHANGED PersonRepository.get | asset_face.personGroupId in (person where person.ownerId = _) -> asset.ownerId = _"]
    );
}

#[test]
fn literals_ctes_and_dynamic_predicates() {
    let src = "get(ps: any[]) { return this.db.with('recent', (db) => db.selectFrom('asset').where('asset.visibility', '!=', sql.lit('hidden'))).selectFrom('recent').where((eb) => eb.and(ps)).execute(); }";
    let m = merak_cli::model(&person_repo(src).into_iter().map(|(k, v)| (k, format!("import {{ sql }} from 'kysely';\n{v}"))).collect()).unwrap();
    let id = "src/person.repository.ts::PersonRepository.get";
    let effects: Vec<String> = m.summaries[id].effects.keys().map(|e| e.render()).collect();
    assert_eq!(effects, ["db_read asset"], "the CTE `recent` is not a table");
    let mut filters: Vec<&str> = m.entities.values().filter(|e| e.id.starts_with(id)).flat_map(|e| &e.filters).map(|f| f.expr.as_str()).collect();
    filters.sort();
    assert_eq!(filters, ["and(`ps`)", "asset.visibility != hidden"]);
    assert!(m.entities.values().all(|e| e.call_shapes.iter().all(|c| !c.shape.contains("lit"))));
}

// ---- M5b′: access requirements ----------------------------------------------

const ACCESS: &str = r#"
export enum Permission { AssetShare = 'asset.share', AssetUpdate = 'asset.update', AlbumRead = 'album.read' }
export class AccessService {
  checkAccess(request: { permission: Permission; ids: string[] }) { return request.ids; }
}
"#;

fn access_app(service: &str) -> Files {
    files(&[("src/access.ts", ACCESS), ("src/memory.service.ts", &format!("import {{ AccessService, Permission }} from './access';\n{service}"))])
}

fn behaviour(before: &Files, after: &Files) -> Vec<String> {
    let t = merak_cli::diff(before, after).unwrap();
    show(&t.ops.into_iter().filter(|o| o.layer != merak_transition::Layer::Structural).collect::<Vec<_>>())
}

#[test]
fn permission_argument_change_is_auth_changed() {
    let svc = |p: &str| {
        format!("export class MemoryService {{ constructor(private access: AccessService) {{}}\n create(ids: string[]) {{ return this.access.checkAccess({{ ids, permission: Permission.{p} }}); }} }}")
    };
    assert_eq!(
        behaviour(&access_app(&svc("AssetShare")), &access_app(&svc("AssetUpdate"))),
        ["AUTH_CHANGED MemoryService.create | permission=asset.share -> permission=asset.update"]
    );
}

#[test]
fn permission_hoisted_to_the_caller_is_not_a_change() {
    // immich 6b7b0fe3a: the helper stopped hard-coding the permission; callers pass it.
    let app = |helper: &str, album: &str, tag: &str| {
        files(&[
            ("src/access.ts", ACCESS),
            ("src/helper.ts", &format!("import {{ AccessService, Permission }} from './access';\nexport function addAssets(access: AccessService, dto: {{ ids: string[]; permission?: Permission }}) {{ return access.checkAccess({{ ids: dto.ids, permission: {helper} }}); }}")),
            (
                "src/services.ts",
                &format!(
                    "import {{ AccessService, Permission }} from './access';\nimport {{ addAssets }} from './helper';
export class AlbumService {{ constructor(private access: AccessService) {{}}\n add(ids: string[]) {{ return addAssets(this.access, {{ ids{album} }}); }} }}
export class TagService {{ constructor(private access: AccessService) {{}}\n add(ids: string[]) {{ return addAssets(this.access, {{ ids{tag} }}); }} }}"
                ),
            ),
        ])
    };
    let before = app("Permission.AssetShare", "", "");
    let after = app("dto.permission!", ", permission: Permission.AssetShare", ", permission: Permission.AssetUpdate");
    assert_eq!(
        behaviour(&before, &after),
        ["AUTH_CHANGED addAssets | permission=asset.share -> permission=_", "AUTH_CHANGED TagService.add | permission=asset.share -> permission=asset.update"]
    );
}

#[test]
fn decorator_only_change_is_seen() {
    let ctl = |opts: &str| {
        format!(
            "import {{ Controller, Get }} from '@nestjs/common';
import {{ Authenticated }} from './auth';
@Controller('albums')
export class AlbumController {{
  @Authenticated({opts})
  @Get() list() {{ return []; }}
}}"
        )
    };
    let app = |opts: &str| files(&[("src/auth.ts", "export const Authenticated = (o: any) => o;"), ("src/album.controller.ts", &ctl(opts))]);
    let t = merak_cli::diff(&app("{ permission: 'album.read' }"), &app("{ public: true }")).unwrap();
    let ops: Vec<String> = t.ops.iter().map(|o| format!("{} {} {:?}", o.kind, o.before.as_deref().unwrap_or(""), o.affects)).collect();
    assert_eq!(ops, [r#"AUTH_WIDENED permission=album.read ["GET /albums"]"#]);
}

#[test]
fn logging_is_reported_apart_from_behaviour() {
    let src = |msg: &str| {
        files(&[("src/log.ts", "export class LoggingRepository { warn(...a: any[]) {} }"), ("src/f.ts", &format!("import {{ LoggingRepository }} from './log';\nexport class S {{ constructor(private logger: LoggingRepository) {{}}\n f() {{ console.log({msg}); this.logger.warn({msg}); return 1; }} }}"))])
    };
    let t = merak_cli::diff(&src("'a'"), &src("'b', 1")).unwrap();
    let got: Vec<String> = t.ops.iter().map(|o| format!("{} {}", o.kind, o.before.as_deref().unwrap_or(""))).collect();
    assert_eq!(got, [r#"LOGGING_CHANGED LoggingRepository.warn("a"); console.log("a")"#]);
}

// ---- M5c: false PURE_REFACTOR claims found against labeled commits ----------

#[test]
fn condition_added_around_a_call_is_not_a_refactor() {
    // immich 64a2fc825: the sleep only happens while the lock is not held.
    let src = |body: &str| files(&[("src/w.ts", &format!("export async function wait(isLocked: boolean) {{ while (true) {{ {body} }} }}"))]);
    let t = merak_cli::diff(&src("await sleep(1000);"), &src("if (!isLocked) { await sleep(1000); }")).unwrap();
    let kinds: Vec<&str> = t.ops.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(kinds, ["UNCLASSIFIED_CHANGE"], "{:#?}", t.ops);
    assert_eq!(t.ops[0].after.as_deref(), Some(r#"sleep("1000") when isLocked is not set"#));
}

#[test]
fn changed_condition_in_a_callback_is_not_a_refactor() {
    // immich 9e5dcc598: an empty `assetIds` array is no longer rejected.
    let src = |cond: &str| {
        files(&[(
            "src/s.ts",
            &format!(
                "export function check(dto: {{ assetIds?: string[] }}, ctx: any) {{ if ({cond}) {{ ctx.addIssue('assetIds not allowed'); }} return true; }}"
            ),
        )])
    };
    let t = merak_cli::diff(&src("dto.assetIds"), &src("dto.assetIds && dto.assetIds.length > 0")).unwrap();
    assert!(t.ops.iter().all(|o| o.kind != "PURE_REFACTOR"), "{:#?}", t.ops);
    assert!(t.ops.iter().any(|o| o.kind == "UNCLASSIFIED_CHANGE"), "{:#?}", t.ops);
}

#[test]
fn continue_rewrite_is_still_a_refactor() {
    // immich e66f2c761: a lint rule turns `if (c) { … }` in loops into `if (!c) continue; …`.
    let before = "export function f(xs: string[], seen: Set<string>) { for (const x of xs) { if (!seen.has(x)) { save(x); } } }";
    let after = "export function f(xs: string[], seen: Set<string>) { for (const x of xs) { if (seen.has(x)) { continue; } save(x); } }";
    assert_eq!(kinds(&files(&[("src/f.ts", before)]), &files(&[("src/f.ts", after)])), ["PURE_REFACTOR *"]);
}

#[test]
fn guarded_promise_sleep_is_not_a_refactor() {
    // immich 64a2fc825, verbatim shape.
    let src = |body: &str| {
        files(&[("src/w.ts", &format!("export async function wait(isLocked: boolean) {{ while (!isLocked) {{ isLocked = await lock(); {body} }} }}"))])
    };
    let t = merak_cli::diff(
        &src("await new Promise((resolve) => setTimeout(resolve, 1000));"),
        &src("if (!isLocked) { await new Promise((resolve) => setTimeout(resolve, 1000)); }"),
    )
    .unwrap();
    assert_eq!(t.ops.iter().map(|o| o.kind.as_str()).collect::<Vec<_>>(), ["UNCLASSIFIED_CHANGE"], "{:#?}", t.ops);
}

// ---- M6: new code is described ----------------------------------------------

#[test]
fn new_method_is_described_by_its_behaviour() {
    let repo = "import { Kysely } from 'kysely';
export class PersonRepository { constructor(private db: Kysely<any>) {}
  reassign(ownerId: string) { return this.db.updateTable('asset_face').where('asset_face.ownerId', '=', ownerId).execute(); }
}";
    let svc = |extra: &str| {
        format!(
            "import {{ PersonRepository }} from './person.repository';
export class PersonService {{ constructor(private repo: PersonRepository, private access: any) {{}}
  get(id: string) {{ return id; }}
  {extra}
}}"
        )
    };
    let merge = "merge(ids: string[]) { if (ids.length < 2) { throw new Error('need two'); } this.access.checkAccess({ ids, permission: 'person.merge' }); return this.repo.reassign(ids[0]); }";
    let app = |extra: &str| files(&[("src/person.repository.ts", repo), ("src/person.service.ts", &svc(extra))]);
    let t = merak_cli::diff(&app(""), &app(merge)).unwrap();
    let described: Vec<String> = t
        .ops
        .iter()
        .filter(|o| o.kind == "BEHAVIOUR_ADDED")
        .map(|o| format!("{} | {}", o.subject.rsplit("::").next().unwrap(), o.after.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(described, ["PersonService.merge | effects: db_write asset_face; requires: permission=person.merge; guards: 2 <= ids.length"], "{:#?}", t.ops);
}

#[test]
fn new_code_without_behaviour_is_not_described() {
    let app = |extra: &str| files(&[("src/m.ts", &format!("export function a(x: number) {{ return x + 1; }}\n{extra}"))]);
    let t = merak_cli::diff(&app(""), &app("export function b(x: number) { return a(x) * 2; }")).unwrap();
    assert!(t.ops.iter().all(|o| o.kind != "BEHAVIOUR_ADDED"), "{:#?}", t.ops);
}

#[test]
fn reads_inside_access_checks_are_not_part_of_a_description() {
    let src = |extra: &str| {
        files(&[(
            "src/s.ts",
            &format!(
                "import {{ Kysely }} from 'kysely';
export class AccessRepository {{ constructor(private db: Kysely<any>) {{}}
  owns(ids: string[]) {{ return this.db.selectFrom('album').where('album.id', 'in', ids).execute(); }} }}
export class BaseService {{ constructor(protected access: AccessRepository, protected db: Kysely<any>) {{}}
  async requireAccess(ids: string[]) {{ const ok = await this.access.owns(ids); if (!ok) {{ throw new Error('forbidden'); }} }} }}
export class TagService extends BaseService {{
  {extra}
}}
export class Controller {{ constructor(private tags: TagService) {{}}
  {}
}}",
                if extra.is_empty() { "" } else { "add(ids: string[]) { return this.tags.add(ids); }" }
            ),
        )])
    };
    let t = merak_cli::diff(
        &src(""),
        &src("async add(ids: string[]) { await this.requireAccess(ids); return this.db.insertInto('tag_asset').values({}).execute(); }"),
    )
    .unwrap();
    let described: Vec<String> = t
        .ops
        .iter()
        .filter(|o| o.kind == "BEHAVIOUR_ADDED")
        .map(|o| format!("{} | {}", o.subject.rsplit("::").next().unwrap(), o.after.as_deref().unwrap()))
        .collect();
    assert_eq!(
        described,
        [
            "Controller.add | effects: db_write tag_asset; validates: BaseService.requireAccess",
            "TagService.add | effects: db_write tag_asset; validates: BaseService.requireAccess"
        ]
    );
}

// ---- M6b: validation schemas ------------------------------------------------

fn schema_ops(before: &str, after: &str) -> Vec<String> {
    let src = |s: &str| {
        files(&[("src/dtos/tag.dto.ts", &format!("import {{ z }} from 'zod';\nconst stringToBool = z.enum(['true', 'false']);\nexport const TagCreateSchema = z.object({{ {s} }}).describe('Create a tag');"))])
    };
    let t = merak_cli::diff(&src(before), &src(after)).unwrap();
    t.ops
        .iter()
        .map(|o| {
            format!("{} {} | {} -> {}", o.kind, o.subject.rsplit("::").next().unwrap(), o.before.as_deref().unwrap_or(""), o.after.as_deref().unwrap_or(""))
                .replace("|  ->", "| ->")
        })
        .collect()
}

#[test]
fn schema_constraint_added_is_a_schema_change() {
    // immich 1b3aa9cd5: tag names may no longer contain '/'.
    assert_eq!(
        schema_ops("name: z.string().describe('Tag name')", "name: z.string().regex(/^[^/]*$/, 'no slash').describe('Tag name')"),
        [r#"SCHEMA_FIELD_CHANGED TagCreateSchema.name | string() -> string().regex(/^[^/]*$/, "no slash")"#]
    );
}

#[test]
fn schema_fields_added_and_removed() {
    assert_eq!(
        schema_ops(
            "name: z.string(), color: z.string().optional()",
            "name: z.string(), page: z.coerce.number().int().min(1).optional(), isUpcoming: stringToBool.optional()"
        ),
        [
            "SCHEMA_FIELD_ADDED TagCreateSchema.isUpcoming | -> stringToBool.optional()",
            "SCHEMA_FIELD_ADDED TagCreateSchema.page | -> coerce.number().int().min(1).optional()",
            "SCHEMA_FIELD_REMOVED TagCreateSchema.color | string().optional() -> ",
        ]
    );
}

#[test]
fn schema_documentation_is_not_a_change() {
    // immich 7abd625af / da8131d5c: only descriptions and examples changed.
    let before = "at: z.string().describe('Time bucket in YYYY-MM-DD').meta({ example: '2024-01-01' })";
    let after = "at: z\n  .string()\n  .describe('Time bucket in YYYY-MM-DDT00:00:00.000Z')\n  .meta({ example: '2024-01-01T00:00:00.000Z' })";
    assert_eq!(schema_ops(before, after), Vec::<String>::new());
}

#[test]
fn schema_shape_moved_into_a_const_is_not_a_change() {
    // immich 8b3d6b320: fields moved into `const shape = {…}`; only strictness changed.
    let src = |s: &str| files(&[("src/dtos/search.dto.ts", &format!("import {{ z }} from 'zod';\n{s}"))]);
    let before = src("export const BranchSchema = z.object({ city: z.string(), rating: z.number() }).partial();");
    let after = src("const branchShape = { city: z.string(), rating: z.number() };\nexport const BranchSchema = z.strictObject(branchShape).partial();");
    let t = merak_cli::diff(&before, &after).unwrap();
    let ops: Vec<String> = t
        .ops
        .iter()
        .map(|o| {
            format!("{} {} | {} -> {}", o.kind, o.subject.rsplit("::").next().unwrap(), o.before.as_deref().unwrap_or(""), o.after.as_deref().unwrap_or(""))
        })
        .collect();
    assert_eq!(ops, ["SCHEMA_CHANGED BranchSchema | object({…}).partial() -> strictObject({…}).partial()"]);
}

#[test]
fn closures_in_a_schema_are_named_after_it() {
    let src = |c: &str| {
        files(&[("src/dtos/link.dto.ts", &format!("import {{ z }} from 'zod';\nexport const LinkSchema = z.object({{ ids: z.array(z.string()) }}).superRefine((dto, ctx) => {{ if ({c}) {{ ctx.addIssue('no'); }} }});"))])
    };
    let t = merak_cli::diff(&src("dto.ids"), &src("dto.ids && dto.ids.length > 0")).unwrap();
    let subjects: std::collections::BTreeSet<&str> = t.ops.iter().map(|o| o.subject.rsplit("::").next().unwrap()).collect();
    assert_eq!(subjects.into_iter().collect::<Vec<_>>(), ["LinkSchema.superRefine"], "{:#?}", t.ops);
}

#[test]
fn schema_built_from_a_base_schema_keeps_its_fields() {
    // immich d6d094738: `MemoryCreateSchema = MemoryCreateBaseSchema.superRefine(…)`.
    let src = |s: &str| files(&[("src/dtos/memory.dto.ts", &format!("import {{ z }} from 'zod';\n{s}"))]);
    let before = src("const MemoryCreateSchema = z.object({ type: z.string(), memoryAt: z.string() });");
    let after = src("const MemoryCreateBaseSchema = z.object({ type: z.string(), memoryAt: z.string() });\nconst MemoryCreateSchema = MemoryCreateBaseSchema.extend({ seenAt: z.string().optional() }).superRefine((dto, ctx) => { if (!dto.type) { ctx.addIssue('x'); } });");
    let t = merak_cli::diff(&before, &after).unwrap();
    let ops: Vec<String> =
        t.ops.iter().filter(|o| o.kind.starts_with("SCHEMA")).map(|o| format!("{} {}", o.kind, o.subject.rsplit("::").next().unwrap())).collect();
    assert_eq!(ops, ["SCHEMA_CHANGED MemoryCreateSchema", "SCHEMA_FIELD_ADDED MemoryCreateSchema.seenAt"]);
}

// ---- Claude Code hooks --------------------------------------------------------

#[test]
fn hooks_report_a_turns_behaviour_change_once() {
    use merak_cli::hook::{prompt, stop, Outcome};
    let dir = std::env::temp_dir().join(format!("merak-hook-{}", std::process::id()));
    let (proj, data) = (dir.join("proj"), dir.join("data"));
    std::fs::create_dir_all(proj.join("src")).unwrap();
    let policy = |roles: &str| format!("export class OrderPolicy {{ canCancel(user: {{ role: string }}): boolean {{ return {roles}; }} }}");
    std::fs::write(proj.join("src/policy.ts"), policy("user.role === 'ADMIN'")).unwrap();
    let input = |active: bool| serde_json::json!({"session_id": "s1", "cwd": proj, "stop_hook_active": active});

    prompt(&input(false), &data).unwrap();
    assert!(matches!(stop(&input(false), &data).unwrap(), Outcome::Quiet), "no edits, nothing to say");

    std::fs::write(proj.join("src/policy.ts"), policy("user.role === 'ADMIN' || user.role === 'MANAGER'")).unwrap();
    let Outcome::Review(msg) = stop(&input(false), &data).unwrap() else { panic!("expected a review") };
    assert!(msg.contains("AUTH_WIDENED OrderPolicy.canCancel: user.role ∈ {ADMIN} → user.role ∈ {ADMIN, MANAGER}"), "{msg}");
    assert!(matches!(stop(&input(true), &data).unwrap(), Outcome::Quiet), "once per turn");
    let _ = std::fs::remove_dir_all(&dir);
}
