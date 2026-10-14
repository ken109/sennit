//! バイナリを実際に動かして、壊れると困る振る舞いを固定する。
//!
//! 単体テストは補助関数の形を見るだけで、`apply` が何をするかは見ていない。
//! 実際 --dry-run が書き込む、相対 --root が自分を指すリンクを作る、退避の
//! 記録が消える、といった不具合は 98 件のテストを 1 つも落とさずに再現できた。
//! ここでは $HOME を隔離して、コマンドの結果をファイルシステムで確かめる。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sennit")
}

struct Repo {
    root: PathBuf,
    home: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sennit-it-{name}"));
        // 前回の実行が権限を狭めたまま終わっていると remove_dir_all が通らない。
        // テストが落ちた後にもう一度走らせられないのは、それ自体が困る。
        if base.exists() {
            let _ = Command::new("chmod")
                .arg("-R")
                .arg("u+rwX")
                .arg(&base)
                .status();
        }
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("repo");
        let home = base.join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(root.join("packages.toml"), "[packages]\n").unwrap();
        Repo { root, home }
    }

    fn write(&self, rel: &str, body: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn write_home(&self, rel: &str, body: &str) {
        let p = self.home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn manifest(&self, body: &str) {
        std::fs::write(self.root.join("sennit.toml"), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .arg("--root")
            .arg(&self.root)
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .output()
            .unwrap()
    }

    /// カレントディャレクトリをリポジトリに置いたまま、相対の --root で走らせる
    fn run_relative(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .current_dir(&self.root)
            .arg("--root")
            .arg(".")
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .output()
            .unwrap()
    }

    fn home_path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    fn root_path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// --dry-run は symlink も生成物も書かない。
///
/// プロバイダと復号コマンドを呼ばないことは
/// dry_run_calls_neither_a_provider_nor_a_decryption_command で見る。
#[test]
fn dry_run_writes_nothing() {
    let r = Repo::new("dry-run");
    r.manifest(
        r#"
[link]
common = ["a.conf", "gen.conf"]
ignore = ["*.tmpl"]

[render]
"gen.conf" = "gen.conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("a.conf", "plain\n");
    r.write("gen.conf.tmpl", "x = {{ bg }}\n");

    let out = ok(&r.run(&["apply", "--dry-run"]));
    assert!(out.contains("nothing written"), "{out}");
    // 生成物がリポジトリに出来ていない
    assert!(!r.root_path("gen.conf").exists(), "dry-run wrote gen.conf");
    // $HOME に何も置かれていない
    assert_eq!(std::fs::read_dir(&r.home).unwrap().count(), 0);
    // これから起きることの件数は本番と一致する
    assert!(out.contains("2 change(s)"), "{out}");
}

/// 相対の --root でも、リンク先は絶対パスになる。
///
/// 相対のままだと a.conf -> ./a.conf という自分自身を指すリンクになり、
/// 元のファイルは退避された後で二度と読めなくなる。
#[test]
fn a_relative_root_does_not_produce_a_self_link() {
    let r = Repo::new("relative-root");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "from the repo\n");

    ok(&r.run_relative(&["apply"]));
    let target = std::fs::read_link(r.home_path("a.conf")).unwrap();
    assert!(target.is_absolute(), "link target is relative: {target:?}");
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "from the repo\n"
    );
}

/// 退避の記録は次の apply でも残り、rollback が効く。
#[test]
fn a_backup_survives_a_later_apply() {
    let r = Repo::new("backup-survives");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    r.write("b.conf", "repo b\n");
    r.write_home("a.conf", "MINE\n");

    ok(&r.run(&["apply"]));
    // 宣言を足して 2 回目
    r.manifest("[link]\ncommon = [\"a.conf\", \"b.conf\"]\n");
    ok(&r.run(&["apply"]));

    let out = ok(&r.run(&["rollback"]));
    assert!(out.contains("1 file(s) restored"), "{out}");
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "MINE\n"
    );
}

/// 同じ行き先に退避が 2 つあっても、古い方を消さない。
#[test]
fn rollback_does_not_destroy_an_older_backup() {
    let r = Repo::new("rollback-two");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    r.write_home("a.conf", "FIRST\n");

    ok(&r.run(&["apply"]));
    // 利用者がリンクを外して自分のファイルを置き直した
    std::fs::remove_file(r.home_path("a.conf")).unwrap();
    r.write_home("a.conf", "SECOND\n");
    ok(&r.run(&["apply"]));

    ok(&r.run(&["rollback"]));
    // 最も新しいものが戻る
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "SECOND\n"
    );
    // 古い方はファイルとして残っている
    let first = std::fs::read_dir(&r.home)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| std::fs::read_to_string(e.path()).unwrap_or_default() == "FIRST\n");
    assert!(first, "the older backup was destroyed");
}

/// フックが落ちても、退避したファイルの在り処は失われない。
#[test]
fn a_failing_hook_does_not_strand_a_backup() {
    let r = Repo::new("hook-fails");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n\n[hooks.boom]\nrun = \"exit 1\"\n");
    r.write("a.conf", "repo\n");
    r.write_home("a.conf", "MINE\n");

    let out = r.run(&["apply"]);
    assert!(!out.status.success(), "the failing hook should fail apply");

    ok(&r.run(&["rollback"]));
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "MINE\n"
    );
}

/// prune が消すのは、自分が張ったリンクだけ。
#[test]
fn prune_leaves_a_foreign_link_alone() {
    let r = Repo::new("prune-foreign");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    ok(&r.run(&["apply"]));

    // 利用者が管理をやめて、自分で別の場所へリンクを張り直した
    std::fs::remove_file(r.home_path("a.conf")).unwrap();
    let elsewhere = r.home.join("elsewhere.conf");
    std::fs::write(&elsewhere, "theirs\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, r.home_path("a.conf")).unwrap();

    r.manifest("[link]\ncommon = []\n");
    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("left alone"), "{out}");
    assert_eq!(
        std::fs::read_link(r.home_path("a.conf")).unwrap(),
        elsewhere
    );
}

/// prune が消すのは、自分が張ったリンク。宣言から外れたら消える。
#[test]
fn prune_removes_a_link_it_made() {
    let r = Repo::new("prune-own");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    ok(&r.run(&["apply"]));
    assert!(r.home_path("a.conf").exists());

    r.manifest("[link]\ncommon = []\n");
    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("prune"), "{out}");
    assert!(std::fs::symlink_metadata(r.home_path("a.conf")).is_err());
}

/// ignore を書き忘れても、テンプレート本体は $HOME に置かれない。
#[test]
fn a_template_is_never_placed_even_without_ignore() {
    let r = Repo::new("no-ignore");
    // ignore を意図的に書かない
    r.manifest(
        r#"
[link]
common = ["conf"]

[render]
"conf/app.conf" = "conf/app.conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("conf/app.conf.tmpl", "x = {{ bg }}\n");

    ok(&r.run(&["apply"]));
    assert!(
        std::fs::symlink_metadata(r.home_path("conf/app.conf.tmpl")).is_err(),
        "the template itself was placed in $HOME"
    );
    assert_eq!(
        std::fs::read_to_string(r.home_path("conf/app.conf")).unwrap(),
        "x = zzz\n"
    );
}

/// 宣言したモードは、ただ張っただけのファイルにも効く。
#[test]
fn a_declared_mode_is_applied_to_a_plain_link() {
    let r = Repo::new("modes");
    r.manifest("[link]\ncommon = [\".npmrc\"]\n\n[modes]\n\".npmrc\" = \"600\"\n");
    r.write(".npmrc", "token\n");
    std::fs::set_permissions(
        r.root_path(".npmrc"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
    )
    .unwrap();

    ok(&r.run(&["apply"]));
    assert_eq!(mode_of(&r.root_path(".npmrc")), 0o600);
    // verify も同じ判断をする
    let out = r.run(&["verify"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 生成物は読み取り専用で置かれる。
#[test]
fn generated_files_are_read_only() {
    let r = Repo::new("read-only");
    r.manifest(
        r#"
[link]
common = ["gen.conf"]
ignore = ["*.tmpl"]

[render]
"gen.conf" = "gen.conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("gen.conf.tmpl", "x = {{ bg }}\n");

    ok(&r.run(&["apply"]));
    assert_eq!(mode_of(&r.root_path("gen.conf")), 0o444);
}

/// まだ生成していないものにリンクを張らない。
///
/// 秘密を読むテンプレートは --secrets を渡すまで飛ばされる。その状態で
/// 張ると、行き先の無いリンクが $HOME に残る。
#[test]
fn a_deferred_output_is_not_linked() {
    let r = Repo::new("deferred");
    r.manifest(
        r#"
[link]
common = ["secret.conf"]
ignore = ["*.tmpl"]

[render]
"secret.conf" = "secret.conf.tmpl"

[providers.never]
command = "false {}"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("secret.conf.tmpl", "token = {{ never://a/b }}\n");

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("deferred"), "{out}");
    assert!(
        std::fs::symlink_metadata(r.home_path("secret.conf")).is_err(),
        "a link was made to a file that does not exist"
    );
    // 「最新です」とは言わない
    assert!(!out.contains("already up to date"), "{out}");
}

/// 他の OS 向けに括ってある秘密は、このマシンでは要らない。
#[test]
fn a_secret_behind_a_false_condition_does_not_defer() {
    let r = Repo::new("guarded-secret");
    r.manifest(
        r#"
[link]
common = ["gen.conf"]
ignore = ["*.tmpl"]

[render]
"gen.conf" = "gen.conf.tmpl"

[providers.never]
command = "false {}"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write(
        "gen.conf.tmpl",
        "x = {{ bg }}\n{{ if sennit.os == \"no-such-os\" }}t = {{ never://a/b }}\n{{ end }}",
    );

    let out = ok(&r.run(&["apply"]));
    assert!(!out.contains("deferred"), "{out}");
    assert_eq!(
        std::fs::read_to_string(r.home_path("gen.conf")).unwrap(),
        "x = zzz\n"
    );
}

/// リポジトリの外を指す宣言は、配置を始める前に断る。
#[test]
fn a_declaration_that_escapes_is_refused() {
    let r = Repo::new("escape");
    r.manifest("[link]\ncommon = [\"../outside\"]\n");
    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(".."),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 読めないディレクトリは失敗にする。黙って飛ばすと、その下のリンクが
/// 「宣言から外れた」ものとして $HOME から消える。
#[test]
fn an_unreadable_directory_fails_instead_of_pruning() {
    use std::os::unix::fs::PermissionsExt;
    // root では権限が効かないので飛ばす
    if euid() == 0 {
        return;
    }
    let r = Repo::new("unreadable");
    r.manifest("[link]\ncommon = [\"conf\"]\n");
    r.write("conf/a.conf", "a\n");
    r.write("conf/b.conf", "b\n");
    ok(&r.run(&["apply"]));

    std::fs::set_permissions(r.root_path("conf"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = r.run(&["apply"]);
    std::fs::set_permissions(r.root_path("conf"), std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!out.status.success(), "an unreadable directory should fail");
    // リンクは残っている
    assert!(std::fs::symlink_metadata(r.home_path("conf/a.conf")).is_ok());
    assert!(std::fs::symlink_metadata(r.home_path("conf/b.conf")).is_ok());
}

/// std に euid が無いので id -u で代用する。
fn euid() -> u32 {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1000)
}

/// 読めなくなっただけのファイルを「消えた」と読まない。
///
/// Path::exists() は EACCES も false を返す。それを「宣言から外れた」と
/// 読むと、権限が変わっただけで $HOME のリンクが消える。
#[test]
fn an_unreadable_file_is_not_treated_as_removed() {
    use std::os::unix::fs::PermissionsExt;
    if euid() == 0 {
        return;
    }
    let r = Repo::new("unreadable-file");
    r.manifest("[link]\ncommon = [\"conf\"]\n");
    r.write("conf/sub/b.conf", "b\n");
    ok(&r.run(&["apply"]));

    // readdir は通るが stat が通らない状態にする
    std::fs::set_permissions(
        r.root_path("conf/sub"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let out = r.run(&["apply"]);
    std::fs::set_permissions(
        r.root_path("conf/sub"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    assert!(
        !out.status.success(),
        "an unreadable file should be an error, not a prune: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        std::fs::symlink_metadata(r.home_path("conf/sub/b.conf")).is_ok(),
        "the link was pruned"
    );
}

/// rollback は何度打っても同じ結果になる。
///
/// 古い退避を記録に残していた頃は、2 度目が今戻したファイルの上に
/// 古い方を書き、3 つあれば 2 つが消えた。
#[test]
fn rollback_is_idempotent() {
    let r = Repo::new("rollback-twice");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");

    for body in ["FIRST\n", "SECOND\n", "THIRD\n"] {
        if r.home_path("a.conf").exists()
            || std::fs::symlink_metadata(r.home_path("a.conf")).is_ok()
        {
            std::fs::remove_file(r.home_path("a.conf")).unwrap();
        }
        r.write_home("a.conf", body);
        ok(&r.run(&["apply"]));
    }

    ok(&r.run(&["rollback"]));
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "THIRD\n"
    );
    let out = ok(&r.run(&["rollback"]));
    assert!(out.contains("nothing to roll back"), "{out}");
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "THIRD\n"
    );
    // 古い 2 つはファイルとして残っている
    let bodies: Vec<String> = std::fs::read_dir(&r.home)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    for want in ["FIRST\n", "SECOND\n"] {
        assert!(bodies.iter().any(|b| b == want), "{want:?} was destroyed");
    }
}

/// 生成物の置き場が symlink なら断る。リンクを辿って書くと、
/// リポジトリの外のファイルを黙って潰せる。
#[test]
fn a_generated_output_is_not_written_through_a_symlink() {
    let r = Repo::new("symlink-output");
    r.manifest(
        r#"
[link]
common = ["gen.conf"]
ignore = ["*.tmpl"]

[render]
"gen.conf" = "gen.conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("gen.conf.tmpl", "x = {{ bg }}\n");

    let victim = r.home.parent().unwrap().join("victim");
    std::fs::write(&victim, "PRECIOUS\n").unwrap();
    std::os::unix::fs::symlink(&victim, r.root_path("gen.conf")).unwrap();

    let out = r.run(&["apply"]);
    assert!(
        !out.status.success(),
        "writing through a symlink should fail"
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS\n");
}

/// --dry-run の 1 行ずつの予告が、本番と食い違わない。
#[test]
fn dry_run_previews_what_apply_will_link() {
    let r = Repo::new("dry-run-preview");
    r.manifest(
        r#"
[link]
common = ["a.conf", "gen.conf"]
ignore = ["*.tmpl"]

[render]
"gen.conf" = "gen.conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("a.conf", "plain\n");
    r.write("gen.conf.tmpl", "x = {{ bg }}\n");

    let preview = ok(&r.run(&["apply", "--dry-run"]));
    assert!(
        !preview.contains("not generated yet"),
        "dry-run said it cannot link a file it also said it would render:\n{preview}"
    );
    let real = ok(&r.run(&["apply"]));
    assert!(real.contains("2 link(s) updated"), "{real}");
}

/// 自分が入れなくなるモードをディレクトリに掛けない。
#[test]
fn a_directory_mode_that_locks_you_out_is_refused() {
    let r = Repo::new("dir-mode");
    r.manifest("[link]\ncommon = [\".ssh\"]\n\n[modes]\n\".ssh\" = \"600\"\n");
    r.write(".ssh/config", "Host x\n");

    let out = r.run(&["apply"]);
    assert!(
        !out.status.success(),
        "600 on a directory should be refused"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_ne!(
        std::fs::metadata(r.root_path(".ssh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// rollback は、戻す先に居るファイルを潰さない。
///
/// apply のあとに利用者がそこへ書いたものは、退避の記録には無い。
/// その上に rename すると、退避を戻すコマンドが別のファイルを消す。
#[test]
fn rollback_does_not_destroy_what_is_at_the_destination() {
    let r = Repo::new("rollback-dest");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    r.write_home("a.conf", "ORIGINAL\n");
    ok(&r.run(&["apply"]));

    // 利用者がリンクを外して自分で書いた
    std::fs::remove_file(r.home_path("a.conf")).unwrap();
    r.write_home("a.conf", "NEW WORK\n");

    ok(&r.run(&["rollback"]));
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "ORIGINAL\n"
    );
    let bodies: Vec<String> = std::fs::read_dir(&r.home)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    assert!(
        bodies.iter().any(|b| b == "NEW WORK\n"),
        "what was at the destination was destroyed: {bodies:?}"
    );
}

/// 退避を取った直後に落ちても、記録は残っている。
#[test]
fn a_backup_is_recorded_before_anything_else_can_fail() {
    let r = Repo::new("backup-early");
    // a.conf は退避され、そのあと zz/b.conf の配置が失敗する
    r.manifest("[link]\ncommon = [\"a.conf\", \"zz\"]\n");
    r.write("a.conf", "repo\n");
    r.write("zz/b.conf", "b\n");
    r.write_home("a.conf", "MINE\n");
    // $HOME/zz を実体のファイルにしておくと、その下に作れない
    r.write_home("zz", "not a directory\n");

    let out = r.run(&["apply"]);
    assert!(!out.status.success(), "apply should have failed");

    ok(&r.run(&["rollback"]));
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "MINE\n"
    );
}

/// 生成物の書き先がディレクトリなら、権限を触る前に断る。
#[test]
fn a_generated_output_is_not_written_over_a_directory() {
    let r = Repo::new("dir-output");
    r.manifest(
        r#"
[link]
common = ["gen.conf"]
ignore = ["*.tmpl"]

[render]
"gen.conf" = "gen.conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("gen.conf.tmpl", "x = {{ bg }}\n");
    std::fs::create_dir_all(r.root_path("gen.conf")).unwrap();
    std::fs::write(r.root_path("gen.conf/inner"), "inner\n").unwrap();

    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    // 中身が読めなくなっていない
    assert_eq!(
        mode_of(&r.root_path("gen.conf")) & 0o700,
        0o700,
        "the directory was chmod-ed before the refusal"
    );
    assert!(r.root_path("gen.conf/inner").exists());
}

/// --no-backup は apply 経由でも、退避せずに消す。
#[test]
fn no_backup_replaces_without_keeping_a_copy() {
    let r = Repo::new("no-backup-apply");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    r.write_home("a.conf", "MINE\n");

    let out = ok(&r.run(&["apply", "--no-backup"]));
    assert!(out.contains("replace"), "{out}");
    assert_eq!(
        std::fs::read_to_string(r.home_path("a.conf")).unwrap(),
        "repo\n"
    );
    let out = ok(&r.run(&["rollback"]));
    assert!(out.contains("nothing to roll back"), "{out}");
}

/// 宣言したモードが違えば verify が落ちる。
#[test]
fn verify_fails_on_a_wrong_mode() {
    use std::os::unix::fs::PermissionsExt;
    let r = Repo::new("verify-mode");
    r.manifest("[link]\ncommon = [\".npmrc\"]\n\n[modes]\n\".npmrc\" = \"600\"\n");
    r.write(".npmrc", "token\n");
    ok(&r.run(&["apply"]));

    std::fs::set_permissions(
        r.root_path(".npmrc"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let out = r.run(&["verify"]);
    assert!(!out.status.success(), "verify should fail on a wrong mode");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("declared 600"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// 暗号文を復号して置く。鍵が無ければ保留する。
#[test]
fn an_encrypted_file_is_decrypted_and_kept_private() {
    let r = Repo::new("encrypted");
    r.write("secret.age", "PLAINTEXT\n");
    // 復号コマンドは中身をそのまま出すだけのもので代用する
    r.manifest(
        r#"
[link]
common = ["secret"]
ignore = ["*.age"]

[encrypted]
"secret" = "secret.age"

[encryption]
command = "cat {}"
"#,
    );

    ok(&r.run(&["apply"]));
    assert_eq!(
        std::fs::read_to_string(r.home_path("secret")).unwrap(),
        "PLAINTEXT\n"
    );
    assert_eq!(mode_of(&r.root_path("secret")), 0o400);
}

/// 鍵が宣言されていて存在しないなら、失敗ではなく保留。
#[test]
fn an_encrypted_file_is_deferred_without_its_key() {
    let r = Repo::new("encrypted-nokey");
    r.write("secret.age", "PLAINTEXT\n");
    r.manifest(
        r#"
[link]
common = ["secret"]
ignore = ["*.age"]

[encrypted]
"secret" = "secret.age"

[encryption]
command = "cat {}"
identity = "/nonexistent/key.txt"
"#,
    );

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("no decryption key"), "{out}");
    assert!(std::fs::symlink_metadata(r.home_path("secret")).is_err());
}

/// --secrets を渡すとプロバイダが呼ばれ、結果は本人だけが読める。
#[test]
fn a_secret_is_fetched_with_secrets_and_written_private() {
    let r = Repo::new("secrets");
    r.manifest(
        r#"
[link]
common = ["conf"]
ignore = ["*.tmpl"]

[render]
"conf" = "conf.tmpl"

[providers.fake]
command = "echo {}"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("conf.tmpl", "token = {{ fake://hello }}\n");

    ok(&r.run(&["apply", "--secrets"]));
    assert_eq!(
        std::fs::read_to_string(r.home_path("conf")).unwrap(),
        "token = hello\n"
    );
    assert_eq!(mode_of(&r.root_path("conf")), 0o400);
}

/// 宣言が無いときの既定の `op` は、`op://` を付けた参照で呼ぶ。
///
/// `{}` は `://` の後ろに置き換わるので、既定の command 側で `op://` を補う。
/// 補わないと実物の `op read` が "secret reference should start with 'op://'" で落ちる。
/// 実物の `op` は呼ばず、PATH の先頭に置いた偽の `op` が引数を検査する。
#[test]
fn the_default_op_provider_passes_an_op_scheme_reference() {
    use std::os::unix::fs::PermissionsExt;

    let r = Repo::new("default-op");
    r.manifest(
        r#"
[link]
common = ["conf"]
ignore = ["*.tmpl"]

[render]
"conf" = "conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("conf.tmpl", "token = {{ op://Vault/Item/field }}\n");

    let fakebin = r.root.parent().unwrap().join("fakebin");
    std::fs::create_dir_all(&fakebin).unwrap();
    let op = fakebin.join("op");
    // 実物と同じく、op:// で始まらない参照は断る。
    std::fs::write(
        &op,
        "#!/bin/sh\n\
         [ \"$1\" = read ] && [ \"$2\" = --no-newline ] || { echo \"unexpected args: $*\" >&2; exit 2; }\n\
         case \"$3\" in\n\
           op://*) printf 'got:%s' \"$3\" ;;\n\
           *) echo \"secret reference should start with 'op://': $3\" >&2; exit 1 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&op, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}",
        fakebin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin())
        .env("PATH", path)
        .arg("--root")
        .arg(&r.root)
        .arg("--home")
        .arg(&r.home)
        .args(["apply", "--secrets"])
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(
        std::fs::read_to_string(r.home_path("conf")).unwrap(),
        "token = got:op://Vault/Item/field\n"
    );
}

/// 宣言の無い scheme は、名指しで断る。
#[test]
fn an_undeclared_scheme_is_named() {
    let r = Repo::new("unknown-scheme");
    r.manifest(
        r#"
[link]
common = ["conf"]
ignore = ["*.tmpl"]

[render]
"conf" = "conf.tmpl"

[providers.fake]
command = "echo {}"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("conf.tmpl", "token = {{ nosuch://x }}\n");

    let out = r.run(&["apply", "--secrets"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nosuch"), "{err}");
    assert!(err.contains("fake"), "{err}");
}

/// フックが実際に走った回は「最新です」と言わない。
#[test]
fn apply_does_not_claim_nothing_happened_after_running_a_hook() {
    let r = Repo::new("hook-ran");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n\n[hooks.always]\nrun = \"true\"\n");
    r.write("a.conf", "repo\n");
    ok(&r.run(&["apply"]));

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("hook"), "{out}");
    assert!(
        !out.contains("already up to date"),
        "a hook ran, so something happened:\n{out}"
    );
}

/// 締め出すモードは preview の時点で断る。
#[test]
fn dry_run_refuses_a_directory_mode_that_locks_you_out() {
    let r = Repo::new("dry-run-dir-mode");
    r.manifest("[link]\ncommon = [\".ssh\"]\n\n[modes]\n\".ssh\" = \"600\"\n");
    r.write(".ssh/config", "Host x\n");

    let out = r.run(&["apply", "--dry-run"]);
    assert!(
        !out.status.success(),
        "the preview should refuse what the real apply refuses"
    );
}

/// 途中のディレクトリが symlink でも、リポジトリの外へは書かない。
///
/// symlink_metadata が見るのは最後の 1 要素だけ。`out -> /outside` を
/// 置くだけで、宣言の検査を回り込んで外のファイルを潰せていた。
#[test]
fn a_generated_output_cannot_escape_through_a_symlinked_parent() {
    let r = Repo::new("symlink-parent");
    let outside = r.home.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("gen.conf"), "PRECIOUS\n").unwrap();

    r.manifest(
        r#"
[link]
common = ["out"]
ignore = ["*.tmpl"]

[render]
"out/gen.conf" = "gen.conf.tmpl"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("gen.conf.tmpl", "x = {{ bg }}\n");
    std::os::unix::fs::symlink(&outside, r.root_path("out")).unwrap();

    let out = r.run(&["apply"]);
    assert!(
        !out.status.success(),
        "writing outside the repo should fail"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("gen.conf")).unwrap(),
        "PRECIOUS\n"
    );
}

/// ディレクトリから所有者の読みを落とすモードも断る。
#[test]
fn a_directory_mode_without_read_is_refused() {
    let r = Repo::new("dir-mode-read");
    r.manifest("[link]\ncommon = [\".ssh\"]\n\n[modes]\n\".ssh\" = \"300\"\n");
    r.write(".ssh/config", "Host x\n");

    let out = r.run(&["apply"]);
    assert!(
        !out.status.success(),
        "300 on a directory should be refused"
    );
    // 掛かっていない
    assert_ne!(mode_of(&r.root_path(".ssh")) & 0o500, 0);
}

/// モードで落ちても、張ったリンクは記録に残る。
///
/// 記録が無いと、次の apply はそのリンクを知らないので prune もできず、
/// 宣言から外しても $HOME に残り続ける。
#[test]
fn links_are_recorded_even_if_a_later_step_fails() {
    let r = Repo::new("record-before-modes");
    r.manifest("[link]\ncommon = [\"a.conf\", \".ssh\"]\n\n[modes]\n\".ssh\" = \"600\"\n");
    r.write("a.conf", "repo\n");
    r.write(".ssh/config", "Host x\n");

    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    assert!(std::fs::symlink_metadata(r.home_path("a.conf")).is_ok());

    // 宣言から外せば prune できる = 記録されていた
    r.manifest("[link]\ncommon = []\n");
    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("prune"), "the link was never recorded:\n{out}");
    assert!(std::fs::symlink_metadata(r.home_path("a.conf")).is_err());
}

/// 読めない宣言パスを verify が「問題なし」と言わない。
///
/// apply は同じ宣言について「stat できない」で落ちる。片方が拒み、
/// 片方が承認するなら、宣言した制限は誰にも適用されないまま通る。
#[test]
fn verify_does_not_approve_a_mode_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;
    if euid() == 0 {
        return;
    }
    let r = Repo::new("verify-unreadable");
    r.manifest("[link]\ncommon = []\n\n[modes]\n\"scripts/tool\" = \"600\"\n");
    r.write("scripts/tool", "#!/bin/sh\n");

    std::fs::set_permissions(
        r.root_path("scripts"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let out = r.run(&["verify"]);
    std::fs::set_permissions(
        r.root_path("scripts"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    assert!(
        !out.status.success(),
        "verify approved a mode it could not read:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// 配置の途中で落ちても、そこまでに張ったリンクは記録される。
///
/// 記録されないと、次の apply はそのリンクを知らない。宣言から外しても
/// prune の対象にならず、誰も管理していないリンクが $HOME に残る。
#[test]
fn a_link_placed_before_a_failure_is_still_recorded() {
    let r = Repo::new("record-midloop");
    // a.conf は張れるが、そのあと zz/ を作るところで落ちる
    r.manifest("[link]\ncommon = [\"a.conf\", \"zz\"]\n");
    r.write("a.conf", "repo\n");
    r.write("zz/b.conf", "b\n");
    r.write_home("zz", "a file, not a directory\n");

    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    assert!(std::fs::symlink_metadata(r.home_path("a.conf")).is_ok());

    // 宣言から外せば prune できる = 記録されていた
    r.manifest("[link]\ncommon = []\n");
    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("prune"), "the link was never recorded:\n{out}");
    assert!(std::fs::symlink_metadata(r.home_path("a.conf")).is_err());
}

/// 退避が消えているなら「戻した」と言わない。
#[test]
fn rollback_does_not_claim_to_restore_a_backup_that_is_gone() {
    let r = Repo::new("rollback-gone");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    r.write_home("a.conf", "MINE\n");
    ok(&r.run(&["apply"]));

    // 利用者が .sennit-backup を掃除した
    std::fs::remove_file(r.home_path("a.conf.sennit-backup")).unwrap();

    let out = ok(&r.run(&["rollback", "--dry-run"]));
    assert!(!out.contains("1 file(s) would be restored"), "{out}");
    let out = ok(&r.run(&["rollback"]));
    assert!(!out.contains("1 file(s) restored"), "{out}");
    assert!(
        out.contains("already gone") || out.contains("nothing to put back"),
        "{out}"
    );
}

/// ignore の前方一致は要素単位。名前の頭が同じだけの別のディレクトリを
/// 巻き込むと、そこは配置されず、次の apply が「宣言から外れた」と読んで
/// $HOME から消す。
#[test]
fn ignore_does_not_swallow_a_sibling_with_a_shared_name_prefix() {
    let r = Repo::new("ignore-sibling");
    r.manifest("[link]\ncommon = [\"conf\"]\nignore = [\"conf/nvim\"]\n");
    r.write("conf/nvim/init.lua", "-- ignored\n");
    r.write("conf/nvim-extra/x.lua", "-- kept\n");

    ok(&r.run(&["apply"]));
    assert!(
        std::fs::symlink_metadata(r.home_path("conf/nvim-extra/x.lua")).is_ok(),
        "a sibling sharing the prefix was ignored"
    );
    assert!(std::fs::symlink_metadata(r.home_path("conf/nvim/init.lua")).is_err());

    // 2 回目で消されない
    ok(&r.run(&["apply"]));
    assert!(std::fs::symlink_metadata(r.home_path("conf/nvim-extra/x.lua")).is_ok());
}

/// --dry-run はプロバイダも復号コマンドも呼ばない。
///
/// どちらも人にロック解除を求めたり、任意のコマンドを走らせたりする。
/// 「何も起きない」と言ったコマンドがそれを出すのは筋が通らない。
#[test]
fn dry_run_calls_neither_a_provider_nor_a_decryption_command() {
    let r = Repo::new("dry-run-no-exec");
    // 呼ばれたら痕跡が残るコマンドにする
    let marker = r.home.parent().unwrap().join("called");
    r.manifest(&format!(
        r#"
[link]
common = ["gen.conf", "secret"]
ignore = ["*.tmpl", "*.age"]

[render]
"gen.conf" = "gen.conf.tmpl"

[encrypted]
"secret" = "secret.age"

[encryption]
command = "sh -c 'echo decrypt >> {m}; cat' {{}}"

[providers.fake]
command = "sh -c 'echo provider >> {m}; echo v'"
"#,
        m = marker.display()
    ));
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("gen.conf.tmpl", "t = {{ fake://a }}\n");
    r.write("secret.age", "PLAIN\n");

    ok(&r.run(&["apply", "--dry-run", "--secrets"]));
    assert!(
        !marker.exists(),
        "--dry-run ran: {}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
}

/// [link] の対象が重なっていても、同じパスを二度張ろうとしない。
#[test]
fn overlapping_link_targets_do_not_break_the_first_apply() {
    let r = Repo::new("overlap");
    r.manifest("[link]\ncommon = [\"conf\", \"conf/nvim\"]\n");
    r.write("conf/nvim/init.lua", "-- x\n");
    r.write("conf/zzz.conf", "z\n");

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("2 link(s) updated"), "{out}");
    assert!(std::fs::symlink_metadata(r.home_path("conf/nvim/init.lua")).is_ok());
    assert!(std::fs::symlink_metadata(r.home_path("conf/zzz.conf")).is_ok());
}

/// 宣言したモードのパスが symlink なら、リンク先を触らずに断る。
#[test]
fn a_declared_mode_does_not_follow_a_symlink_out_of_the_repository() {
    use std::os::unix::fs::PermissionsExt;
    let r = Repo::new("mode-symlink");
    let outside = r.home.parent().unwrap().join("secret");
    std::fs::write(&outside, "key\n").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();

    r.manifest("[link]\ncommon = []\n\n[modes]\n\"keylink\" = \"644\"\n");
    std::os::unix::fs::symlink(&outside, r.root_path("keylink")).unwrap();

    let out = r.run(&["apply"]);
    assert!(
        !out.status.success(),
        "a symlinked mode target should be refused"
    );
    assert_eq!(mode_of(&outside), 0o600, "the link target was chmod-ed");
}

/// 読めなくなった「親」を、宣言から外れたものとして扱わない。
///
/// 対象そのものではなくその上のディレクトリが読めなくなる場合、対象の
/// stat は通るので、対象を狭めるテストでは捕まらない。
#[test]
fn an_unreadable_ancestor_does_not_prune() {
    use std::os::unix::fs::PermissionsExt;
    if euid() == 0 {
        return;
    }
    let r = Repo::new("unreadable-ancestor");
    r.manifest("[link]\ncommon = [\"conf/nvim\"]\n");
    r.write("conf/nvim/init.lua", "-- x\n");
    ok(&r.run(&["apply"]));

    std::fs::set_permissions(r.root_path("conf"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = r.run(&["apply"]);
    std::fs::set_permissions(r.root_path("conf"), std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !out.status.success(),
        "an unreadable ancestor should be an error, not a prune:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        std::fs::symlink_metadata(r.home_path("conf/nvim/init.lua")).is_ok(),
        "the link was pruned"
    );
}

/// 宣言したモードは、途中のディレクトリが symlink でもリポジトリの外へ
/// 出ない。最後の 1 要素だけ見ていた頃は素通りしていた。
#[test]
fn a_declared_mode_cannot_escape_through_a_symlinked_parent() {
    use std::os::unix::fs::PermissionsExt;
    let r = Repo::new("mode-symlink-parent");
    let outside = r.home.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let victim = outside.join("victim");
    std::fs::write(&victim, "key\n").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();

    r.manifest("[link]\ncommon = []\n\n[modes]\n\"out/victim\" = \"777\"\n");
    std::os::unix::fs::symlink(&outside, r.root_path("out")).unwrap();

    let out = r.run(&["apply"]);
    assert!(
        !out.status.success(),
        "should refuse to chmod outside the repo"
    );
    assert_eq!(mode_of(&victim), 0o600, "the file outside was chmod-ed");
}

/// コメントアウトした設定を、生きている宣言として読まない。
#[test]
fn commented_out_settings_are_not_requirements() {
    let r = Repo::new("comments");
    r.manifest("[link]\ncommon = []\n");
    std::fs::write(r.root_path("packages.toml"), "[packages]\n").unwrap();
    r.write(
        ".config/alacritty/alacritty.toml",
        "[font.normal]\n# family = \"JetBrains Mono\"\nfamily = \"Hack Nerd Font Mono\"\n",
    );
    r.write(
        ".config/zed/settings.json",
        "{\n  // \"buffer_font_family\": \"Zed Plex Mono\",\n  \"auto_install_extensions\": {\n    // \"html\": true,\n    \"toml\": true,\n  },\n}\n",
    );

    let out = r.run(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout);
    for phantom in ["JetBrains Mono", "Zed Plex Mono", "html"] {
        assert!(
            !text.contains(phantom),
            "{phantom} came from a comment:\n{text}"
        );
    }
    // 生きている宣言は拾えている
    assert!(text.contains("Hack Nerd Font Mono"), "{text}");
    assert!(text.contains("toml"), "{text}");
}

/// まだ生成されていない出力にモードを宣言していても、apply は落ちない。
///
/// 秘密を読むテンプレートは --secrets を渡すまで生成されないので、初回や
/// CI では親ディレクトリごと存在しない。これを異常扱いにすると、その状態が
/// 常態であるコンテナで一度も apply が通らなくなる。
#[test]
fn a_declared_mode_on_a_deferred_output_does_not_fail_the_run() {
    let r = Repo::new("mode-deferred");
    r.manifest(
        r#"
[link]
common = ["a.conf", "gh"]
ignore = ["*.tmpl"]

[render]
"gh/hosts.yml" = "hosts.yml.tmpl"

[modes]
"gh/hosts.yml" = "600"

[providers.never]
command = "false {}"
"#,
    );
    r.write("theme.toml", "bg = \"zzz\"\n");
    r.write("a.conf", "plain\n");
    r.write("hosts.yml.tmpl", "token = {{ never://a/b }}\n");

    let preview = ok(&r.run(&["apply", "--dry-run"]));
    assert!(preview.contains("nothing written"), "{preview}");

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("link(s) updated"), "{out}");
    assert!(std::fs::symlink_metadata(r.home_path("a.conf")).is_ok());
}

// ---- verify --secrets ----

/// 検査対象の秘密を `cat` で読むリポジトリを用意する。参照は値ではなく
/// ファイルのパスなので、失敗の表示に参照が出ても秘密の値は出ない。
fn probe_repo(name: &str, secret: &str, probe_body: &str) -> Repo {
    let r = Repo::new(name);
    r.write("vault/token", secret);
    let token = r.root_path("vault/token");
    r.manifest(&format!(
        r#"
[link]
common = []

[providers.file]
command = "cat {{}}"

[probes.svc]
secret = "file://{}"
{probe_body}
"#,
        token.display()
    ));
    r
}

fn both(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// 通常の verify は秘密に触れない。op のロック解除や外部への通信を、
/// 頼まれていないのに起こさない。
#[test]
fn plain_verify_does_not_fetch_or_probe_secrets() {
    let r = probe_repo("probe-off", "tok\n", "command = \"false\"");
    let out = r.run(&["verify"]);
    let text = both(&out);
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("probed"), "{text}");
}

/// 検査コマンドが 0 で終われば有効。値は標準入力に届く。
#[test]
fn verify_secrets_accepts_a_secret_the_check_accepts() {
    let r = probe_repo(
        "probe-ok",
        "good-token\n",
        "command = \"grep -qx 'Bearer good-token'\"\ninput = \"Bearer {}\"",
    );
    let out = r.run(&["verify", "--secrets"]);
    let text = both(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("probed 1 secret(s): 1 accepted"), "{text}");
    assert!(!text.contains("good-token"), "{text}");
}

/// 拒否された秘密は名前で報告して失敗する。値は画面に出ない。
/// 検査コマンドが値を標準出力に吐いても、sennit はそれを読まない。
#[test]
fn verify_secrets_fails_on_a_rejected_secret_without_printing_it() {
    let r = probe_repo(
        "probe-rejected",
        "s3cr3t-LEAKCHECK\n",
        "command = \"sh -c 'cat; cat >&2; exit 1'\"",
    );
    let out = r.run(&["verify", "--secrets"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("invalid"), "{text}");
    assert!(text.contains("svc"), "{text}");
    assert!(text.contains("1 secret(s) rejected"), "{text}");
    assert!(!text.contains("s3cr3t-LEAKCHECK"), "secret leaked:\n{text}");
}

/// 値を取れなかったものを「問題なし」にしない。ロックされたままの
/// 1Password を相手にした CI が、何も確かめずに緑になる。
#[test]
fn verify_secrets_fails_when_the_secret_cannot_be_read() {
    let r = Repo::new("probe-unreadable");
    r.manifest(
        r#"
[link]
common = []

[providers.never]
command = "false {}"

[probes.svc]
secret = "never://a/b"
command = "true"
"#,
    );
    let out = r.run(&["verify", "--secrets"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("unchecked"), "{text}");
    assert!(text.contains("could not be checked"), "{text}");
}

/// invalid-exit に無い失敗は「失効」ではなく「確かめられなかった」。
#[test]
fn verify_secrets_does_not_call_an_unlisted_failure_a_revocation() {
    let r = probe_repo(
        "probe-unlisted",
        "tok\n",
        "command = \"sh -c 'exit 7'\"\ninvalid-exit = [22]",
    );
    let out = r.run(&["verify", "--secrets"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("unchecked"), "{text}");
    // 理由の文に invalid-exit という語が出るので、見るのは結果の印のほう
    assert!(!text.contains("\x1b[31minvalid"), "{text}");
    assert!(!text.contains("rejected"), "{text}");
}

/// 宣言が無ければ、何も確かめていないと言う。黙って「ok」とは言わない。
#[test]
fn verify_secrets_says_so_when_nothing_is_declared() {
    let r = Repo::new("probe-none");
    r.manifest("[link]\ncommon = []\n");
    let out = r.run(&["verify", "--secrets"]);
    let text = both(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("no [probes] declared"), "{text}");
}

/// パッケージ側の失敗と秘密側の失敗は、どちらも出す。片方が隠れると、
/// 直して再実行するたびに次の失敗が出てくる。
#[test]
fn verify_secrets_reports_machine_and_secret_failures_together() {
    let r = probe_repo("probe-both", "tok\n", "command = \"false\"");
    // 宣言したモードと食い違うファイルを足す
    let mut m = std::fs::read_to_string(r.root_path("sennit.toml")).unwrap();
    m.push_str("\n[modes]\n\"vault/token\" = \"600\"\n");
    r.manifest(&m);
    std::fs::set_permissions(
        r.root_path("vault/token"),
        std::os::unix::fs::PermissionsExt::from_mode(0o644),
    )
    .unwrap();

    let out = r.run(&["verify", "--secrets"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("mode"), "{text}");
    assert!(text.contains("invalid"), "{text}");
}

/// 参照の形になっていない secret は、宣言を読んだ時点で断る。
#[test]
fn a_probe_whose_secret_is_not_a_reference_is_refused() {
    let r = Repo::new("probe-bad-secret");
    r.manifest(
        r#"
[link]
common = []

[probes.svc]
secret = "plain-token"
command = "true"
"#,
    );
    let out = r.run(&["verify"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("scheme://"), "{}", both(&out));
}

// ---------------------------------------------------------------------------
// ignore globs, and pruning the walk
// ---------------------------------------------------------------------------

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

#[test]
fn ignore_globs_keep_tests_and_dependencies_out_of_home() {
    let r = Repo::new("ignore-globs");
    r.manifest(
        r#"
[link]
common = [".claude"]
ignore = ["**/*.test.ts", ".claude/node_modules", ".claude/worktrees", ".claude/package.json", "*.tar.gz"]
"#,
    );
    r.write(".claude/CLAUDE.md", "keep\n");
    r.write(".claude/hooks/guard.ts", "keep\n");
    r.write(".claude/hooks/guard.test.ts", "test\n");
    r.write(".claude/hooks/deep/more.test.ts", "test\n");
    r.write(".claude/node_modules/dep/index.js", "dep\n");
    r.write(".claude/worktrees/wt/file.md", "wt\n");
    r.write(".claude/package.json", "{}\n");
    r.write(".claude/skills/x/bundle.tar.gz", "zip\n");

    ok(&r.run(&["apply"]));
    assert!(exists(&r.home_path(".claude/CLAUDE.md")));
    assert!(exists(&r.home_path(".claude/hooks/guard.ts")));
    for gone in [
        ".claude/hooks/guard.test.ts",
        ".claude/hooks/deep/more.test.ts",
        ".claude/node_modules",
        ".claude/worktrees",
        ".claude/package.json",
        ".claude/skills/x/bundle.tar.gz",
    ] {
        assert!(!exists(&r.home_path(gone)), "{gone} was placed");
    }

    // and a second apply does not prune or relink anything
    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("already up to date"), "{out}");
}

/// An ignored directory is not entered, so something unreadable inside it
/// cannot fail the plan.
#[test]
fn an_unreadable_directory_inside_an_ignored_tree_does_not_fail_apply() {
    use std::os::unix::fs::PermissionsExt;
    if euid() == 0 {
        return;
    }
    let r = Repo::new("ignored-unreadable");
    r.manifest("[link]\ncommon = [\".claude\"]\nignore = [\".claude/worktrees\"]\n");
    r.write(".claude/CLAUDE.md", "keep\n");
    r.write(".claude/worktrees/wt/inner/file.md", "wt\n");
    let locked = r.root_path(".claude/worktrees/wt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = r.run(&["apply"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(exists(&r.home_path(".claude/CLAUDE.md")));
}

// ---------------------------------------------------------------------------
// a link above the destination that points into the repository
// ---------------------------------------------------------------------------

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

/// Every file name under `dir` that looks like a sennit backup.
fn backups_under(dir: &Path) -> Vec<String> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".sennit-backup"))
        .collect()
}

/// The accident this guards against: an older layout linked a whole skill
/// directory into the repository. Following that link, the destination of
/// `SKILL.md` *is* the repository's `SKILL.md`, so apply used to move the
/// repository's own file aside and link it to itself.
#[test]
fn a_directory_link_into_the_repository_becomes_per_file_links() {
    let r = Repo::new("ancestor-relink");
    r.manifest("[link]\ncommon = [\".claude\"]\n");
    r.write(".claude/CLAUDE.md", "top\n");
    r.write(".claude/skills/gog/SKILL.md", "gog skill\n");
    r.write(".claude/skills/gog/ref/a.md", "ref\n");
    std::fs::create_dir_all(r.home_path(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(
        r.root_path(".claude/skills/gog"),
        r.home_path(".claude/skills/gog"),
    )
    .unwrap();

    // the plan says what will happen, and changes nothing
    let diff = ok(&r.run(&["diff"]));
    assert!(diff.contains("relink-parent"), "{diff}");
    let list = ok(&r.run(&["list"]));
    assert!(list.contains("relink-parent"), "{list}");
    let dry = ok(&r.run(&["apply", "--dry-run"]));
    assert!(dry.contains("relink-parent"), "{dry}");
    assert!(std::fs::symlink_metadata(r.home_path(".claude/skills/gog"))
        .unwrap()
        .file_type()
        .is_symlink());

    ok(&r.run(&["apply"]));

    // the repository is exactly as it was
    assert_eq!(
        read(&r.root_path(".claude/skills/gog/SKILL.md")),
        "gog skill\n"
    );
    assert_eq!(read(&r.root_path(".claude/skills/gog/ref/a.md")), "ref\n");
    assert!(
        backups_under(&r.root).is_empty(),
        "{:?}",
        backups_under(&r.root)
    );
    // the directory is real now, and each file is a link to the repository
    let gog = r.home_path(".claude/skills/gog");
    let meta = std::fs::symlink_metadata(&gog).unwrap();
    assert!(meta.is_dir() && !meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(gog.join("SKILL.md")).unwrap(),
        std::fs::canonicalize(r.root_path(".claude/skills/gog/SKILL.md")).unwrap()
    );
    assert_eq!(read(&gog.join("SKILL.md")), "gog skill\n");
    assert_eq!(read(&gog.join("ref/a.md")), "ref\n");
    assert!(backups_under(&r.home).is_empty());

    let again = ok(&r.run(&["apply"]));
    assert!(again.contains("already up to date"), "{again}");
}

/// The whole directory can be the link (`~/.claude -> repo/.claude`).
#[test]
fn a_link_to_the_whole_directory_becomes_a_real_directory() {
    let r = Repo::new("ancestor-relink-whole");
    r.manifest("[link]\ncommon = [\".claude\"]\nignore = [\".claude/worktrees\"]\n");
    r.write(".claude/CLAUDE.md", "top\n");
    r.write(".claude/skills/a/SKILL.md", "a\n");
    r.write(".claude/worktrees/wt/file.md", "wt\n");
    std::os::unix::fs::symlink(r.root_path(".claude"), r.home_path(".claude")).unwrap();

    ok(&r.run(&["apply"]));
    assert_eq!(read(&r.root_path(".claude/CLAUDE.md")), "top\n");
    assert!(backups_under(&r.root).is_empty());
    let meta = std::fs::symlink_metadata(r.home_path(".claude")).unwrap();
    assert!(meta.is_dir() && !meta.file_type().is_symlink());
    assert_eq!(read(&r.home_path(".claude/skills/a/SKILL.md")), "a\n");
    // an ignored tree is no longer reachable from $HOME
    assert!(!exists(&r.home_path(".claude/worktrees")));
}

/// An old layout may have linked a skill directory to another repository
/// (`~/.claude/skills/progress -> life/skills/progress`). Following that link,
/// the destination is a file in the other tree, which apply used to move aside
/// and then fill with a link of its own. Only the symlink may change.
#[test]
fn a_link_that_leaves_the_repository_is_replaced_without_touching_its_target() {
    let r = Repo::new("ancestor-outside");
    r.manifest("[link]\ncommon = [\".claude\"]\n");
    r.write(".claude/CLAUDE.md", "top\n");
    r.write(".claude/skills/progress/SKILL.md", "from dotfiles\n");
    let life = r.home.parent().unwrap().join("life/skills/progress");
    std::fs::create_dir_all(&life).unwrap();
    std::fs::write(life.join("SKILL.md"), "from life\n").unwrap();
    let old_link = r.home_path(".claude/skills/progress");
    std::fs::create_dir_all(r.home_path(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&life, &old_link).unwrap();

    let diff = ok(&r.run(&["diff"]));
    assert!(diff.contains("relink-parent"), "{diff}");

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("relink-parent"), "{out}");
    // the other tree is exactly as it was: no backup, no new link
    assert_eq!(read(&life.join("SKILL.md")), "from life\n");
    assert_eq!(
        std::fs::read_dir(&life).unwrap().count(),
        1,
        "something was written into the other tree"
    );
    assert!(backups_under(life.parent().unwrap().parent().unwrap()).is_empty());
    // here the directory is real and holds a link to the repository's file
    let meta = std::fs::symlink_metadata(&old_link).unwrap();
    assert!(meta.is_dir() && !meta.file_type().is_symlink());
    assert_eq!(read(&old_link.join("SKILL.md")), "from dotfiles\n");
    assert_eq!(
        read(&r.root_path(".claude/skills/progress/SKILL.md")),
        "from dotfiles\n"
    );
    assert!(backups_under(&r.root).is_empty());

    let again = ok(&r.run(&["apply"]));
    assert!(again.contains("already up to date"), "{again}");

    // rollback puts the original symlink back
    ok(&r.run(&["rollback"]));
    assert_eq!(std::fs::read_link(&old_link).unwrap(), life);
    assert_eq!(read(&life.join("SKILL.md")), "from life\n");
    assert_eq!(std::fs::read_dir(&life).unwrap().count(), 1);
}

/// `--no-backup` does not keep the old link; the target is still untouched.
#[test]
fn no_backup_drops_an_outside_link_and_still_leaves_its_target_alone() {
    let r = Repo::new("ancestor-outside-no-backup");
    r.manifest("[link]\ncommon = [\".claude\"]\n");
    r.write(".claude/skills/progress/SKILL.md", "from dotfiles\n");
    let life = r.home.parent().unwrap().join("life-nb/skills/progress");
    std::fs::create_dir_all(&life).unwrap();
    std::fs::write(life.join("SKILL.md"), "from life\n").unwrap();
    std::fs::create_dir_all(r.home_path(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&life, r.home_path(".claude/skills/progress")).unwrap();

    ok(&r.run(&["apply", "--no-backup"]));
    assert_eq!(read(&life.join("SKILL.md")), "from life\n");
    assert!(backups_under(&r.home).is_empty());
    assert_eq!(
        read(&r.home_path(".claude/skills/progress/SKILL.md")),
        "from dotfiles\n"
    );
}

/// A link above something the repository does not declare is none of its business.
#[test]
fn a_link_above_an_undeclared_path_is_left_alone() {
    let r = Repo::new("ancestor-undeclared");
    r.manifest("[link]\ncommon = [\".claude\"]\n");
    r.write(".claude/CLAUDE.md", "top\n");
    let life = r.home.parent().unwrap().join("life-keep/skills/legacy");
    std::fs::create_dir_all(&life).unwrap();
    std::fs::write(life.join("SKILL.md"), "from life\n").unwrap();
    std::fs::create_dir_all(r.home_path(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(&life, r.home_path(".claude/skills/legacy")).unwrap();

    ok(&r.run(&["apply"]));
    assert_eq!(
        std::fs::read_link(r.home_path(".claude/skills/legacy")).unwrap(),
        life
    );
    assert_eq!(read(&life.join("SKILL.md")), "from life\n");
}

// ---------------------------------------------------------------------------
// [merge]
// ---------------------------------------------------------------------------

const MERGE_MANIFEST: &str = r#"
[link]
common = [".claude"]
ignore = ["*.local.json"]

[merge.".claude/settings.json"]
from = ".claude/settings.shared.json"
"#;

const SHARED: &str = r#"{
  "model": "opus",
  "env": { "SHARED": "1" },
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "shared-stop" }] }]
  }
}
"#;

fn merge_repo(name: &str) -> Repo {
    let r = Repo::new(name);
    r.manifest(MERGE_MANIFEST);
    r.write(".claude/CLAUDE.md", "top\n");
    r.write(".claude/settings.shared.json", SHARED);
    r
}

fn json(p: &Path) -> serde_json::Value {
    serde_json::from_str(&read(p)).unwrap()
}

#[test]
fn merge_creates_the_target_and_a_second_apply_writes_nothing() {
    use std::os::unix::fs::MetadataExt;
    let r = merge_repo("merge-create");

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("merge"), "{out}");
    assert!(out.contains(".claude/settings.json"), "{out}");
    let target = r.home_path(".claude/settings.json");
    let meta = std::fs::symlink_metadata(&target).unwrap();
    assert!(
        meta.is_file(),
        "the target is a file of its own, not a link"
    );
    assert_eq!(mode_of(&target), 0o600);
    assert_eq!(json(&target)["model"], "opus");
    // two-space indent, trailing newline
    assert!(
        read(&target).starts_with("{\n  \"model\""),
        "{}",
        read(&target)
    );
    assert!(read(&target).ends_with("}\n"));
    // nothing to back up when there was nothing there
    assert!(backups_under(&r.home).is_empty());

    let (ino, mtime) = (meta.ino(), meta.mtime_nsec());
    let again = ok(&r.run(&["apply"]));
    assert!(again.contains("already up to date"), "{again}");
    let after = std::fs::metadata(&target).unwrap();
    assert_eq!((after.ino(), after.mtime_nsec()), (ino, mtime), "rewritten");
}

#[test]
fn the_merge_source_is_never_linked() {
    let r = merge_repo("merge-source-not-linked");
    ok(&r.run(&["apply"]));
    assert!(exists(&r.home_path(".claude/CLAUDE.md")));
    assert!(!exists(&r.home_path(".claude/settings.shared.json")));
    let list = ok(&r.run(&["list"]));
    assert!(!list.contains("settings.shared.json"), "{list}");
}

#[test]
fn merge_keeps_machine_specific_keys_and_replaces_arrays() {
    let r = merge_repo("merge-rules");
    r.write_home(
        ".claude/settings.json",
        r#"{
  "zeta": "machine",
  "env": { "LOCAL": "1" },
  "model": "sonnet",
  "hooks": {
    "Notification": [{ "hooks": [{ "type": "command", "command": "machine-only" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "old-stop" }] }]
  }
}
"#,
    );
    ok(&r.run(&["apply"]));
    let got = json(&r.home_path(".claude/settings.json"));
    assert_eq!(got["model"], "opus");
    assert_eq!(got["zeta"], "machine");
    assert_eq!(got["env"]["LOCAL"], "1");
    assert_eq!(got["env"]["SHARED"], "1");
    // an event the shared file names is replaced as a whole ...
    assert_eq!(
        got["hooks"]["Stop"][0]["hooks"][0]["command"],
        "shared-stop"
    );
    assert_eq!(got["hooks"]["Stop"].as_array().unwrap().len(), 1);
    // ... and one it does not name survives
    assert_eq!(
        got["hooks"]["Notification"][0]["hooks"][0]["command"],
        "machine-only"
    );
    // the target's own key order comes first
    let text = read(&r.home_path(".claude/settings.json"));
    assert!(
        text.find("\"zeta\"").unwrap() < text.find("\"model\"").unwrap(),
        "{text}"
    );
}

#[test]
fn merge_moves_an_untouched_target_aside_once_and_rollback_restores_it() {
    let r = merge_repo("merge-backup");
    let original = "{\n  \"mine\": true\n}\n";
    r.write_home(".claude/settings.json", original);
    let target = r.home_path(".claude/settings.json");
    let backup = r.home_path(".claude/settings.json.sennit-backup");

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("kept the old file"), "{out}");
    assert_eq!(read(&backup), original);
    assert_eq!(json(&target)["mine"], true);

    // a changed shared file merges again, without another backup
    r.write(
        ".claude/settings.shared.json",
        &SHARED.replace("opus", "haiku"),
    );
    ok(&r.run(&["apply"]));
    assert_eq!(json(&target)["model"], "haiku");
    assert_eq!(read(&backup), original);
    assert!(!r
        .home_path(".claude/settings.json.sennit-backup.1")
        .exists());

    let out = ok(&r.run(&["rollback"]));
    assert!(out.contains("1 file(s) restored"), "{out}");
    assert_eq!(read(&target), original);
}

#[test]
fn no_backup_does_not_keep_a_copy_of_the_target() {
    let r = merge_repo("merge-no-backup");
    r.write_home(".claude/settings.json", "{\"mine\": true}\n");
    ok(&r.run(&["apply", "--no-backup"]));
    assert!(backups_under(&r.home).is_empty());
    assert_eq!(json(&r.home_path(".claude/settings.json"))["mine"], true);
}

#[test]
fn merge_keeps_the_mode_of_an_existing_target() {
    use std::os::unix::fs::PermissionsExt;
    let r = merge_repo("merge-mode");
    r.write_home(".claude/settings.json", "{}\n");
    let target = r.home_path(".claude/settings.json");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    ok(&r.run(&["apply"]));
    assert_eq!(mode_of(&target), 0o640);
}

#[test]
fn merge_dry_run_writes_nothing() {
    let r = merge_repo("merge-dry-run");
    let original = "{\"mine\": true}\n";
    r.write_home(".claude/settings.json", original);

    let out = ok(&r.run(&["apply", "--dry-run"]));
    assert!(out.contains("would merge"), "{out}");
    assert!(out.contains("nothing written"), "{out}");
    assert_eq!(read(&r.home_path(".claude/settings.json")), original);
    assert!(!r.home_path(".claude/settings.json.sennit-backup").exists());
    // no link was placed either
    assert!(!exists(&r.home_path(".claude/CLAUDE.md")));
}

#[test]
fn merge_dry_run_fails_on_a_broken_target() {
    let r = merge_repo("merge-dry-run-broken");
    r.write_home(".claude/settings.json", "{ not json");
    let out = r.run(&["apply", "--dry-run"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("not valid JSON"), "{}", both(&out));
}

#[test]
fn a_broken_target_is_reported_and_left_exactly_as_it_was() {
    for (name, body) in [
        ("garbage", "{ not json"),
        ("jsonc", "{\n  // comment\n  \"a\": 1\n}\n"),
        ("array", "[1, 2]\n"),
        ("empty", ""),
    ] {
        let r = merge_repo(&format!("merge-broken-{name}"));
        r.write_home(".claude/settings.json", body);
        let out = r.run(&["apply"]);
        assert!(!out.status.success(), "{name} should fail");
        assert_eq!(read(&r.home_path(".claude/settings.json")), body, "{name}");
        assert!(backups_under(&r.home).is_empty(), "{name}");
    }
}

#[test]
fn a_symlinked_target_is_refused() {
    let r = merge_repo("merge-symlink-target");
    std::fs::create_dir_all(r.home_path(".claude")).unwrap();
    let real = r.home.join("real-settings.json");
    std::fs::write(&real, "{}\n").unwrap();
    std::os::unix::fs::symlink(&real, r.home_path(".claude/settings.json")).unwrap();
    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("symlink"), "{}", both(&out));
    assert_eq!(read(&real), "{}\n");
}

/// A source that is missing or broken fails the plan, before anything is placed.
#[test]
fn a_bad_merge_source_fails_before_anything_is_placed() {
    let r = merge_repo("merge-bad-source");
    std::fs::remove_file(r.root_path(".claude/settings.shared.json")).unwrap();
    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("does not exist"), "{}", both(&out));
    assert!(!exists(&r.home_path(".claude/CLAUDE.md")));

    r.write(".claude/settings.shared.json", "{ \"a\": ");
    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("not valid JSON"), "{}", both(&out));
    assert!(!exists(&r.home_path(".claude/CLAUDE.md")));
    // diff and list read the same plan
    assert!(!r.run(&["diff"]).status.success());
}

#[test]
fn merge_output_only_names_key_paths() {
    let r = Repo::new("merge-no-values");
    r.manifest(MERGE_MANIFEST);
    r.write(
        ".claude/settings.shared.json",
        "{\"env\": {\"API_TOKEN\": \"hunter2-shared\"}}\n",
    );
    r.write_home(
        ".claude/settings.json",
        "{\"env\": {\"API_TOKEN\": \"hunter2-local\"}}\n",
    );
    let diff = ok(&r.run(&["diff"]));
    let dry = ok(&r.run(&["apply", "--dry-run"]));
    let apply = ok(&r.run(&["apply"]));
    let verify_after_edit = {
        std::fs::write(
            r.home_path(".claude/settings.json"),
            "{\"env\": {\"API_TOKEN\": \"hunter2-edited\"}}\n",
        )
        .unwrap();
        both(&r.run(&["verify"]))
    };
    for text in [&diff, &dry, &apply, &verify_after_edit] {
        assert!(!text.contains("hunter2"), "a value was printed:\n{text}");
    }
    assert!(apply.contains("env.API_TOKEN"), "{apply}");
}

#[test]
fn verify_reports_a_stale_or_missing_merge_target() {
    let r = merge_repo("merge-verify");
    ok(&r.run(&["apply"]));
    let out = ok(&r.run(&["verify"]));
    assert!(out.contains("ok"), "{out}");

    // somebody changed a shared key by hand
    let target = r.home_path(".claude/settings.json");
    let mut v = json(&target);
    v["model"] = serde_json::json!("sonnet");
    std::fs::write(&target, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    let out = r.run(&["verify"]);
    assert!(!out.status.success());
    let text = both(&out);
    assert!(text.contains("stale"), "{text}");
    assert!(text.contains("model"), "{text}");
    // diff and list say so as well
    assert!(ok(&r.run(&["diff"])).contains("stale"));
    assert!(ok(&r.run(&["list"])).contains("stale"));

    // apply puts it right, verify agrees
    ok(&r.run(&["apply"]));
    assert!(r.run(&["verify"]).status.success());

    // keys only the machine has are not drift
    let mut v = json(&target);
    v["machine"] = serde_json::json!(true);
    std::fs::write(&target, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    assert!(r.run(&["verify"]).status.success());

    std::fs::remove_file(&target).unwrap();
    let out = r.run(&["verify"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("missing"), "{}", both(&out));
}

#[test]
fn diff_before_and_after_a_merge() {
    let r = merge_repo("merge-diff");
    let before = ok(&r.run(&["diff"]));
    assert!(before.contains(".claude/settings.json"), "{before}");
    assert!(before.contains("missing"), "{before}");
    ok(&r.run(&["apply"]));
    let after = ok(&r.run(&["diff"]));
    assert!(after.contains("no changes"), "{after}");
}

#[test]
fn a_merge_runs_after_the_links_and_before_the_hooks() {
    let r = Repo::new("merge-order");
    let seen = r.home.join("hook-saw.json");
    r.manifest(&format!(
        "{MERGE_MANIFEST}\n[hooks.after]\nrun = \"cp '{}' '{}'\"\n",
        r.home_path(".claude/settings.json").display(),
        seen.display()
    ));
    r.write(".claude/CLAUDE.md", "top\n");
    r.write(".claude/settings.shared.json", SHARED);
    ok(&r.run(&["apply"]));
    assert_eq!(json(&seen)["model"], "opus");
}

#[test]
fn merging_into_a_directory_that_was_a_link_into_the_repository() {
    let r = merge_repo("merge-after-relink");
    std::os::unix::fs::symlink(r.root_path(".claude"), r.home_path(".claude")).unwrap();
    ok(&r.run(&["apply"]));
    // the settings went to $HOME, not into the repository through the old link
    assert!(!r.root_path(".claude/settings.json").exists());
    assert_eq!(json(&r.home_path(".claude/settings.json"))["model"], "opus");
    assert_eq!(read(&r.root_path(".claude/CLAUDE.md")), "top\n");
}

#[test]
fn removing_a_declaration_leaves_the_target_alone() {
    let r = merge_repo("merge-undeclared");
    ok(&r.run(&["apply"]));
    let target = r.home_path(".claude/settings.json");
    let before = read(&target);

    r.manifest("[link]\ncommon = [\".claude\"]\nignore = [\"*.shared.json\"]\n");
    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("no longer declared"), "{out}");
    assert_eq!(read(&target), before);
    // said once
    let again = ok(&r.run(&["apply"]));
    assert!(!again.contains("no longer declared"), "{again}");
}

#[test]
fn a_merge_output_that_the_repository_also_provides_is_refused() {
    let r = merge_repo("merge-collides-with-link");
    r.write(".claude/settings.json", "{}\n");
    let out = r.run(&["apply"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("[merge] output"), "{}", both(&out));
    assert!(!exists(&r.home_path(".claude/settings.json")));
}

#[test]
fn a_merge_output_declared_under_link_is_refused_when_the_manifest_loads() {
    let r = Repo::new("merge-collides-declared");
    r.manifest(
        "[link]\ncommon = [\".claude/settings.json\"]\n\n[merge.\".claude/settings.json\"]\nfrom = \"s.json\"\n",
    );
    let out = r.run(&["diff"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("[link]"), "{}", both(&out));
}

// ---------------------------------------------------------------------------
// replacing a parent link that also holds sennit's own state
// ---------------------------------------------------------------------------

/// The state file lives under `<home>/.local/state/sennit`. When a link on the
/// way to it is the one being replaced, saving the state used to create the
/// directory the replacement then tried to create, and the first apply failed.
#[test]
fn a_parent_link_that_holds_the_state_file_is_replaced() {
    let r = Repo::new("ancestor-holds-state");
    r.manifest("[link]\ncommon = [\".local\"]\n");
    r.write(".local/state/x.conf", "x\n");
    let outside = r.home.parent().unwrap().join("other-state");
    std::fs::create_dir_all(outside.join("sennit")).unwrap();
    std::fs::write(outside.join("keep-me"), "mine\n").unwrap();
    std::fs::create_dir_all(r.home_path(".local")).unwrap();
    std::os::unix::fs::symlink(&outside, r.home_path(".local/state")).unwrap();

    let out = r.run(&["apply"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let state = r.home_path(".local/state/sennit/state.json");
    assert!(
        read(&state).contains(".local/state/x.conf"),
        "{}",
        read(&state)
    );
    assert_eq!(read(&r.home_path(".local/state/x.conf")), "x\n");
    assert_eq!(read(&outside.join("keep-me")), "mine\n");

    let again = ok(&r.run(&["apply"]));
    assert!(again.contains("already up to date"), "{again}");

    ok(&r.run(&["rollback"]));
    assert_eq!(
        std::fs::read_link(r.home_path(".local/state")).unwrap(),
        outside
    );
}

// ---------------------------------------------------------------------------
// a declared top-level path that is itself a link leading outside
// ---------------------------------------------------------------------------

/// `~/.config -> ~/Dropbox/config` is a deliberate layout. The path the manifest
/// declares is never replaced when it leads outside the repository; files are
/// placed inside it, as in 0.19, and a note says so.
#[test]
fn a_declared_top_level_link_that_leaves_the_repository_is_kept() {
    let r = Repo::new("top-level-outside");
    r.manifest("[link]\ncommon = [\".config\"]\n");
    r.write(".config/app/a.conf", "from repo\n");
    let outside = r.home.parent().unwrap().join("cloud-config");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("theirs.conf"), "theirs\n").unwrap();
    std::os::unix::fs::symlink(&outside, r.home_path(".config")).unwrap();

    let diff = ok(&r.run(&["diff"]));
    assert!(diff.contains("outside the repository"), "{diff}");
    assert!(!diff.contains("relink-parent"), "{diff}");

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("outside the repository"), "{out}");
    assert!(!out.contains("relink-parent"), "{out}");
    // the link stays, and the file is placed inside what it leads to
    assert_eq!(std::fs::read_link(r.home_path(".config")).unwrap(), outside);
    assert_eq!(read(&outside.join("theirs.conf")), "theirs\n");
    assert!(std::fs::symlink_metadata(outside.join("app/a.conf"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(backups_under(&r.home).is_empty());

    let again = ok(&r.run(&["apply"]));
    assert!(again.contains("already up to date"), "{again}");
}

/// Only the top-level path is protected; a link below it is still replaced.
#[test]
fn a_link_below_a_kept_top_level_directory_is_still_replaced() {
    let r = Repo::new("below-top-level");
    r.manifest("[link]\ncommon = [\".config\"]\n");
    r.write(".config/app/a.conf", "from repo\n");
    let other = r.home.parent().unwrap().join("old-app");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("a.conf"), "old\n").unwrap();
    std::fs::create_dir_all(r.home_path(".config")).unwrap();
    std::os::unix::fs::symlink(&other, r.home_path(".config/app")).unwrap();

    ok(&r.run(&["apply"]));
    assert_eq!(read(&other.join("a.conf")), "old\n");
    assert_eq!(read(&r.home_path(".config/app/a.conf")), "from repo\n");
    assert!(std::fs::symlink_metadata(r.home_path(".config/app"))
        .unwrap()
        .is_dir());
}

// ---------------------------------------------------------------------------
// what counts as "inside the repository"
// ---------------------------------------------------------------------------

/// prune removes a link only when it points into the repository. A target that
/// climbs out with `..` starts with the repository's path as text but is not
/// in it.
#[test]
fn prune_leaves_a_link_alone_when_its_target_climbs_out_with_dotdot() {
    let r = Repo::new("prune-dotdot");
    r.manifest("[link]\ncommon = [\"a.conf\"]\n");
    r.write("a.conf", "repo\n");
    ok(&r.run(&["apply"]));

    // the user repointed it, at a path that begins with the repository's path
    let theirs = r.home.parent().unwrap().join("theirs.conf");
    std::fs::write(&theirs, "theirs\n").unwrap();
    std::fs::remove_file(r.home_path("a.conf")).unwrap();
    let sneaky = std::fs::canonicalize(&r.root)
        .unwrap()
        .join("../theirs.conf");
    std::os::unix::fs::symlink(&sneaky, r.home_path("a.conf")).unwrap();

    r.manifest("[link]\ncommon = []\n");
    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("left alone"), "{out}");
    assert!(std::fs::symlink_metadata(r.home_path("a.conf")).is_ok());
    assert_eq!(read(&theirs), "theirs\n");
}

// ---------------------------------------------------------------------------
// a path moving from [link] to [merge]
// ---------------------------------------------------------------------------

/// The old declaration linked `.claude/settings.json`; the new one merges into
/// it. Until apply prunes the old link, previews and checks must treat it as
/// not being there rather than as a symlink they cannot merge into.
#[test]
fn a_path_moving_from_link_to_merge_previews_cleanly() {
    let r = Repo::new("link-to-merge");
    r.manifest("[link]\ncommon = [\".claude\"]\nignore = [\"*.shared.json\"]\n");
    r.write(".claude/settings.json", "{\"old\": true}\n");
    r.write(".claude/settings.shared.json", SHARED);
    ok(&r.run(&["apply"]));
    assert!(
        std::fs::symlink_metadata(r.home_path(".claude/settings.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // the repository no longer holds the file; the manifest merges instead
    std::fs::remove_file(r.root_path(".claude/settings.json")).unwrap();
    r.manifest(MERGE_MANIFEST);

    let diff = r.run(&["diff"]);
    assert!(diff.status.success(), "{}", both(&diff));
    assert!(!both(&diff).contains("symlink"), "{}", both(&diff));
    let dry = r.run(&["apply", "--dry-run"]);
    assert!(dry.status.success(), "{}", both(&dry));
    assert!(both(&dry).contains("would merge"), "{}", both(&dry));
    let verify = r.run(&["verify"]);
    assert!(!both(&verify).contains("symlink"), "{}", both(&verify));
    assert!(both(&verify).contains("missing"), "{}", both(&verify));
    // none of that changed anything
    assert!(
        std::fs::symlink_metadata(r.home_path(".claude/settings.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("prune"), "{out}");
    let target = r.home_path(".claude/settings.json");
    assert!(std::fs::symlink_metadata(&target).unwrap().is_file());
    assert_eq!(json(&target)["model"], "opus");
    assert!(r.run(&["verify"]).status.success());
}

// ---------------------------------------------------------------------------
// [modes] on a merge output
// ---------------------------------------------------------------------------

fn merge_modes_repo(name: &str) -> Repo {
    let r = Repo::new(name);
    r.manifest(&format!(
        "{MERGE_MANIFEST}\n[modes]\n\".claude/settings.json\" = \"640\"\n"
    ));
    r.write(".claude/CLAUDE.md", "top\n");
    r.write(".claude/settings.shared.json", SHARED);
    r
}

/// A mode declared for a merge output applies to the file in `$HOME`, which is
/// not in the repository, and `verify` checks it there.
#[test]
fn a_declared_mode_applies_to_a_merge_output() {
    use std::os::unix::fs::PermissionsExt;
    let r = merge_modes_repo("merge-modes");
    let target = r.home_path(".claude/settings.json");

    ok(&r.run(&["apply"]));
    assert_eq!(
        mode_of(&target),
        0o640,
        "a new target takes the declared mode"
    );
    assert!(r.run(&["verify"]).status.success());

    // loosened by hand: verify notices, a preview says what apply will do
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = r.run(&["verify"]);
    assert!(!out.status.success());
    assert!(both(&out).contains("mode"), "{}", both(&out));
    let dry = ok(&r.run(&["apply", "--dry-run"]));
    assert!(dry.contains("would set"), "{dry}");
    assert_eq!(mode_of(&target), 0o644);

    let out = ok(&r.run(&["apply"]));
    assert!(out.contains("mode"), "{out}");
    assert_eq!(mode_of(&target), 0o640);
    assert!(r.run(&["verify"]).status.success());
}

#[test]
fn a_declared_mode_wins_over_the_mode_of_an_existing_merge_target() {
    use std::os::unix::fs::PermissionsExt;
    let r = merge_modes_repo("merge-modes-existing");
    r.write_home(".claude/settings.json", "{}\n");
    let target = r.home_path(".claude/settings.json");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    ok(&r.run(&["apply"]));
    assert_eq!(mode_of(&target), 0o640);
}

// ---------------------------------------------------------------------------
// merge: not losing the original, and not racing the tool
// ---------------------------------------------------------------------------

/// An older sennit rewrites state.json without the `merged` list. The next run
/// must not mistake the already merged file for the user's original and file it
/// away as a second backup next to the real one.
#[test]
fn a_forgotten_merge_claim_does_not_produce_a_second_backup() {
    let r = merge_repo("merge-forgotten-claim");
    let original = "{\"mine\": true}\n";
    r.write_home(".claude/settings.json", original);
    ok(&r.run(&["apply"]));
    let backup = r.home_path(".claude/settings.json.sennit-backup");
    assert_eq!(read(&backup), original);

    // what 0.19 does to the state: it knows nothing of `merged`
    let state = r.home_path(".local/state/sennit/state.json");
    let mut v: serde_json::Value = serde_json::from_str(&read(&state)).unwrap();
    v.as_object_mut().unwrap().remove("merged");
    std::fs::write(&state, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    r.write(
        ".claude/settings.shared.json",
        &SHARED.replace("opus", "haiku"),
    );
    ok(&r.run(&["apply"]));
    assert_eq!(
        json(&r.home_path(".claude/settings.json"))["model"],
        "haiku"
    );
    assert_eq!(read(&backup), original, "the original was overwritten");
    assert!(!r
        .home_path(".claude/settings.json.sennit-backup.1")
        .exists());
}
