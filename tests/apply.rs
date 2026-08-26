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
