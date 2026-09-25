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
