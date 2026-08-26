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

/// --dry-run は symlink も生成物も書かない。プロバイダも復号も呼ばない。
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
