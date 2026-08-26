mod audit;
mod detect;
mod encrypted;
mod hooks;
mod manifest;
mod packages;
mod plan;
mod render;
mod state;
mod sync;
mod verify;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use manifest::Manifest;
use packages::Packages;
use plan::{Plan, State};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "sennit",
    version,
    about = "Dotfiles manager that keeps symlink semantics"
)]
struct Cli {
    /// Repository root (default: search upwards for sennit.toml)
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Where to place files (default: $HOME)
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render, then place symlinks
    Apply {
        /// Show what would happen without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Delete a real file in the way instead of moving it aside
        #[arg(long)]
        no_backup: bool,
        /// Also render templates that read secrets
        #[arg(long)]
        secrets: bool,
    },
    /// Show what an apply would change
    Diff,
    /// Check that every dependency the configs reference is declared
    Check,
    /// Cross-check declarations against shell history to find unused ones
    Audit {
        /// History file (default: ~/.zsh_history or ~/.bash_history)
        #[arg(long)]
        history: Option<PathBuf>,
    },
    /// Check that everything declared actually resolves on this machine
    Verify {
        /// Write the result as JSON, for comparing machines
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// Diff two `verify --export` reports
    Compare { a: PathBuf, b: PathBuf },
    /// Install declared packages that are missing
    Sync {
        /// Show what would be installed without installing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Render templates and decrypt encrypted files
    Render {
        /// Also render templates that read secrets.
        ///
        /// Skipped by default: a secret manager needs a person to sign in and
        /// unlock it, which cannot happen partway through an unattended install.
        #[arg(long)]
        secrets: bool,
    },
    /// Put back files that the last apply moved aside
    Rollback {
        /// Show what would be restored without restoring it
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the state of every managed path
    List {
        /// Only show entries that need changing
        #[arg(long)]
        changed: bool,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\x1b[31merror\x1b[0m: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = match &cli.root {
        Some(p) => p.clone(),
        None => find_root().context("could not locate sennit.toml")?,
    };
    let home = match &cli.home {
        Some(p) => p.clone(),
        None => PathBuf::from(std::env::var("HOME").context("HOME is not set")?),
    };

    let manifest = Manifest::load(&root.join("sennit.toml"))?;
    let plan = Plan::build(&root, &home, &manifest)?;

    match cli.command {
        Command::Apply {
            dry_run,
            no_backup,
            secrets,
        } => {
            // 生成物はコミットしないので、配置の前に必ず作る
            render_all(&root, &manifest, secrets)?;
            let plan = Plan::build(&root, &home, &manifest)?;
            apply(&plan, &root, &home, &manifest, dry_run, !no_backup)
        }
        Command::Rollback { dry_run } => rollback(&home, dry_run),
        Command::Diff => {
            print_diff(&plan);
            Ok(())
        }
        Command::Check => check(&root),
        Command::Render { secrets } => render_all(&root, &manifest, secrets),
        Command::Sync { dry_run } => sync::sync(&root, dry_run),
        Command::Verify { export } => verify::verify(&root, export),
        Command::Compare { a, b } => verify::compare(&a, &b),
        Command::Audit { history } => audit::audit(&root, history),
        Command::List { changed } => {
            print_list(&plan, changed);
            Ok(())
        }
    }
}

/// カレントディレクトリから上方向に sennit.toml を探す。
fn find_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("sennit.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("sennit.toml not found in any parent directory");
        }
    }
}

/// 設定ファイルが参照している外部依存が packages.toml に宣言されているかを検証する。
///
/// 設定だけ更新してパッケージ側が追随しない、というドリフトを CI で落とすための
/// コマンド。フォントやエディタ拡張も対象にする。実際に踏んだドリフトのうち
/// 半分近くが brew formula 以外だったため。
fn check(root: &Path) -> Result<()> {
    let packages = Packages::load(&root.join("packages.toml"))?;
    let provided = packages.provided();
    let required = detect::scan(root)?;

    let mut missing: Vec<&detect::Requirement> = Vec::new();
    let mut optional: Vec<&detect::Requirement> = Vec::new();
    for req in &required {
        match provided.get(&(req.kind, req.name.clone())) {
            None => missing.push(req),
            Some(true) => optional.push(req),
            Some(false) => {}
        }
    }

    println!(
        "checked {} requirement(s) against {} declared name(s)",
        required.len(),
        provided.len()
    );

    // 逆方向: 宣言してあるのに、どの設定も参照していないもの。
    // 使わなくなった設定を消したときにパッケージだけ残る、というドリフトを拾う。
    // コマンドは設定に現れないまま日常的に使うもの(bat, fd, rg)が多いので、
    // 参照されることが前提のフォントと拡張だけを対象にする。
    let required_names: std::collections::HashSet<_> =
        required.iter().map(|r| (r.kind, r.name.clone())).collect();
    let mut unused: Vec<_> = provided
        .keys()
        .filter(|(k, _)| matches!(k, packages::Kind::Font | packages::Kind::Extension))
        .filter(|key| !required_names.contains(*key))
        .collect();
    unused.sort();

    for (kind, name) in &unused {
        println!(
            "\x1b[33munused\x1b[0m      {:<9} {}  (declared, referenced by nothing)",
            kind.label(),
            name
        );
    }

    for o in &optional {
        println!(
            "\x1b[33moptional\x1b[0m    {:<9} {}  (declared, not installed by setup)",
            o.kind.label(),
            o.name
        );
    }

    if missing.is_empty() {
        println!("\x1b[32mok\x1b[0m  no undeclared dependencies");
        return Ok(());
    }

    println!();
    for m in &missing {
        println!(
            "\x1b[31mundeclared\x1b[0m  {:<9} {}\n            required by {}",
            m.kind.label(),
            m.name,
            m.source
        );
    }
    bail!("{} undeclared dependency(ies)", missing.len());
}

/// theme.toml を単一ソースとして、配色を持つ設定ファイルを生成する。
///
/// 生成物はリポジトリにコミットする。git で差分が見え、sennit check が
/// 生成後のファイルを読め、新規 clone でも設定が揃っているため。
/// 代わりに「テンプレートを直したが生成し忘れる」ことが起きうるので、
/// --check を CI に置いて食い違いを落とす。
fn render_all(root: &Path, manifest: &Manifest, secrets: bool) -> Result<()> {
    decrypt_all(root, manifest)?;
    if manifest.render.is_empty() {
        return Ok(());
    }
    let data: Vec<PathBuf> = if manifest.data.is_empty() {
        vec![root.join("theme.toml")]
    } else {
        manifest.data.iter().map(|d| root.join(d)).collect()
    };
    let vars = render::load_vars(&data)?;
    // 宣言があればそれを、無ければ op だけを既定にする
    let providers = if manifest.providers.is_empty() {
        render::default_providers()
    } else {
        manifest.providers.clone()
    };
    let mut cache = render::SecretCache::with(providers);
    let mut deferred = Vec::new();

    for (out_rel, tmpl_rel) in &manifest.render {
        let tmpl_path = root.join(tmpl_rel);
        let out_path = root.join(out_rel);
        let template = std::fs::read_to_string(&tmpl_path)
            .with_context(|| format!("failed to read template {}", tmpl_path.display()))?;

        // 秘密を参照するものは既定で飛ばす。1Password はサインインと
        // ロック解除を人手に要求するので、初回セットアップや CI、
        // コンテナでは原理的に成立しない。そこを異常扱いにしない。
        if !secrets && render::needs_secrets(&template) {
            deferred.push(out_rel.clone());
            continue;
        }
        let rendered = render::expand_with(&template, &vars, tmpl_rel, &mut cache)?;

        // 中身が同じなら書き直さない。mtime が動くと apply が無駄に張り直す。
        // ただし権限だけは毎回揃える。中身が変わらない限り読み取り専用に
        // ならない、という穴があった。
        if std::fs::read_to_string(&out_path).ok().as_deref() == Some(rendered.as_str()) {
            enforce_mode(
                &out_path,
                manifest.mode_for(Path::new(out_rel)),
                render::needs_secrets(&template),
            );
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let declared = manifest.mode_for(Path::new(out_rel));
        write_rendered(
            &out_path,
            &rendered,
            render::needs_secrets(&template),
            declared,
        )
        .with_context(|| format!("failed to write {}", out_path.display()))?;
        println!("  \x1b[33mrendered\x1b[0m   {out_rel}");
    }

    if !deferred.is_empty() {
        for out_rel in &deferred {
            println!("  \x1b[36mdeferred\x1b[0m   {out_rel}  (needs 1Password)");
        }
        println!(
            "{} template(s) not rendered; run `sennit apply --secrets` once 1Password is unlocked",
            deferred.len()
        );
    }
    Ok(())
}

/// リポジトリ内の暗号文を復号して置く。
///
/// プロバイダと違い外部サービスもサインインも要らないので、鍵さえあれば
/// 無人で動く。鍵が無い環境では保留するが、それは「設定していない」であって
/// 「壊れている」ではない。
fn decrypt_all(root: &Path, manifest: &Manifest) -> Result<()> {
    if manifest.encrypted.is_empty() {
        return Ok(());
    }
    let Some(enc) = &manifest.encryption else {
        bail!("encrypted files are declared but [encryption] is not");
    };

    if !enc.ready() {
        for out_rel in manifest.encrypted.keys() {
            println!("  \x1b[36mdeferred\x1b[0m   {out_rel}  (no decryption key)");
        }
        if let Some(id) = enc.identity_path() {
            println!(
                "{} encrypted file(s) not decrypted; the key is expected at {id}",
                manifest.encrypted.len()
            );
        }
        return Ok(());
    }

    for (out_rel, src_rel) in &manifest.encrypted {
        let src = root.join(src_rel);
        let out_path = root.join(out_rel);
        let plain = enc.decrypt(&src)?;

        // 中身が同じでも権限は揃える
        if std::fs::read(&out_path).ok().as_deref() == Some(plain.as_slice()) {
            enforce_mode(&out_path, manifest.mode_for(Path::new(out_rel)), true);
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if out_path.exists() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o600)).ok();
        }
        std::fs::write(&out_path, &plain)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        // 暗号化してあるということは秘密なので、既定で 0600
        // 復号結果も生成物なので読み取り専用。暗号化してあった＝秘密なので 0400
        let mode = manifest.mode_for(Path::new(out_rel)).unwrap_or(0o400);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
        println!("  \x1b[35mdecrypted\x1b[0m  {out_rel}");
    }
    Ok(())
}

/// 生成物を書く。秘密を含むものは 0600 にする。
///
/// 既定の umask 022 では 0644 になり、トークンが誰でも読める状態で置かれる。
/// 手で管理していた頃の ~/.npmrc は 0600 だったので、これは劣化にあたる。
/// 内容ではなく権限で守る。
/// 生成物を書く。
///
/// 既定で読み取り専用にする。生成物はテンプレートの隣に並ぶので、
/// うっかりそちらを開いて編集してしまう。書けてしまうと次の render で
/// 黙って消える。書けなくしておけばエディタが保存に失敗し、その場で気づく。
///
/// symlink 越しに $HOME から編集した場合も同じく弾かれる。生の編集感を
/// 保つのがこのツールの主張だが、生成物だけは例外であることを権限で示す。
/// 生成物のあるべきモードに揃える。
fn enforce_mode(path: &Path, declared: Option<u32>, secret: bool) {
    use std::os::unix::fs::PermissionsExt;
    let want = declared.unwrap_or(if secret { 0o400 } else { 0o444 });
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o777 != want {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(want));
        }
    }
}

fn write_rendered(path: &Path, contents: &str, secret: bool, declared: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // 前回書いたものが読み取り専用だと上書きできない
    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    std::fs::write(path, contents)?;

    // 宣言があればそれに従う。無ければ読み取り専用。秘密なら本人だけ。
    let mode = declared.unwrap_or(if secret { 0o400 } else { 0o444 });
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn apply(
    plan: &Plan,
    root: &Path,
    home: &Path,
    manifest: &Manifest,
    dry_run: bool,
    backup: bool,
) -> Result<()> {
    let previous = state::State::load(home)?;
    let current: Vec<PathBuf> = plan.entries.iter().map(|e| e.rel.clone()).collect();
    let stale = previous.stale(&current);

    let changes: Vec<_> = plan.changes().collect();
    let nothing_to_link = changes.is_empty() && stale.is_empty();

    let mut backups = Vec::new();

    // 前回張ったが今回の宣言から外れたもの。放っておくと管理をやめた設定の
    // リンクが $HOME に残り続ける。壊れたリンクだけを消し、実体は触らない。
    for rel in &stale {
        let dest = home.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&dest) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        println!("  {:>8}  {}", "prune", rel.display());
        if !dry_run {
            std::fs::remove_file(&dest)
                .with_context(|| format!("failed to remove {}", dest.display()))?;
        }
    }

    for e in &changes {
        let verb = match &e.state {
            State::Missing => "link",
            State::Wrong { .. } => "relink",
            State::Occupied => {
                if backup {
                    "backup"
                } else {
                    "replace"
                }
            }
            State::Linked => unreachable!(),
        };
        println!("  {:>8}  {}", verb, e.rel.display());

        if dry_run {
            continue;
        }

        if let Some(parent) = e.dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if let Some(kept) = remove_dest(&e.dest, &e.state, backup)? {
            backups.push(state::Backup {
                dest: e.dest.clone(),
                kept_at: kept,
            });
        }
        std::os::unix::fs::symlink(&e.src, &e.dest)
            .with_context(|| format!("failed to link {}", e.dest.display()))?;
    }

    // 配置のあとにフックを走らせる。設定を置いてから取り込む処理なので、
    // 順序が逆だと参照先がまだ無い。
    let hooks = hooks::run_all(root, &manifest.hooks, &previous.hooks, dry_run)?;

    if dry_run {
        println!(
            "\n{} change(s), {} prune(s), nothing written (--dry-run)",
            changes.len(),
            stale.len()
        );
        return Ok(());
    }

    if nothing_to_link && hooks == previous.hooks {
        println!("already up to date ({} links)", plan.entries.len());
        return Ok(());
    }

    state::State {
        links: current,
        backups: backups.clone(),
        hooks,
    }
    .save(home)?;

    println!(
        "\n{} link(s) updated, {} pruned",
        changes.len(),
        stale.len()
    );
    if !backups.is_empty() {
        println!(
            "{} file(s) moved aside; `sennit rollback` puts them back",
            backups.len()
        );
    }
    Ok(())
}

/// 直前の apply が退避したファイルを元に戻す。
///
/// 退避した実体を書き戻し、その上に張った symlink を外す。apply 全体を
/// 巻き戻すのではなく、「利用者のファイルを置き換えた」部分だけを戻す。
fn rollback(home: &Path, dry_run: bool) -> Result<()> {
    let mut st = state::State::load(home)?;
    if st.backups.is_empty() {
        println!("nothing to roll back");
        return Ok(());
    }

    for b in &st.backups {
        println!("  {:>8}  {}", "restore", b.dest.display());
        if dry_run {
            continue;
        }
        if !b.kept_at.exists() {
            println!("            the backup is gone; skipped");
            continue;
        }
        // 張った symlink を外してから書き戻す
        if let Ok(meta) = std::fs::symlink_metadata(&b.dest) {
            if meta.file_type().is_symlink() {
                std::fs::remove_file(&b.dest)?;
            }
        }
        std::fs::rename(&b.kept_at, &b.dest)
            .with_context(|| format!("failed to restore {}", b.dest.display()))?;
    }

    if dry_run {
        println!(
            "\n{} file(s) would be restored (--dry-run)",
            st.backups.len()
        );
        return Ok(());
    }
    let n = st.backups.len();
    st.backups.clear();
    st.save(home)?;
    println!("\n{n} file(s) restored");
    Ok(())
}

/// 既存の dest を退ける。
///
/// symlink でない実体は利用者が書いたものかもしれない。既定では消さずに
/// 隣へ退避する。注意書きで防ぐのではなく、消せない作りにしておく。
fn remove_dest(dest: &Path, state: &State, backup: bool) -> Result<Option<PathBuf>> {
    match state {
        State::Missing | State::Linked => Ok(None),
        // 別の場所を指す symlink は、それ自体に中身が無いので消してよい
        State::Wrong { .. } => {
            std::fs::remove_file(dest)
                .with_context(|| format!("failed to remove symlink {}", dest.display()))?;
            Ok(None)
        }
        State::Occupied => {
            if backup {
                let to = backup_path(dest)?;
                std::fs::rename(dest, &to).with_context(|| {
                    format!(
                        "failed to move {} aside to {}",
                        dest.display(),
                        to.display()
                    )
                })?;
                println!("            kept the old file at {}", to.display());
                return Ok(Some(to));
            }
            let meta = std::fs::symlink_metadata(dest)?;
            if meta.is_dir() {
                std::fs::remove_dir_all(dest)
            } else {
                std::fs::remove_file(dest)
            }
            .with_context(|| format!("failed to remove {}", dest.display()))?;
            Ok(None)
        }
    }
}

/// 退避先。既にあれば連番を足して、退避で退避を潰さないようにする。
fn backup_path(dest: &Path) -> Result<PathBuf> {
    let base = format!("{}.sennit-backup", dest.display());
    let first = PathBuf::from(&base);
    if !first.exists() {
        return Ok(first);
    }
    for n in 1..1000 {
        let p = PathBuf::from(format!("{base}.{n}"));
        if !p.exists() {
            return Ok(p);
        }
    }
    bail!("too many backups next to {}", dest.display())
}

fn print_diff(plan: &Plan) {
    let changes: Vec<_> = plan.changes().collect();
    if changes.is_empty() {
        println!("no changes ({} links already in place)", plan.entries.len());
        return;
    }
    for e in &changes {
        match &e.state {
            State::Missing => println!("\x1b[32m+\x1b[0m {}", e.rel.display()),
            State::Wrong { current } => println!(
                "\x1b[33m~\x1b[0m {}\n    now -> {}\n    new -> {}",
                e.rel.display(),
                current.display(),
                e.src.display()
            ),
            State::Occupied => println!(
                "\x1b[31m!\x1b[0m {}  (not a symlink; would be replaced)",
                e.rel.display()
            ),
            State::Linked => {}
        }
    }
    println!("\n{} change(s)", changes.len());
}

fn print_list(plan: &Plan, changed_only: bool) {
    let width = plan
        .entries
        .iter()
        .map(|e| e.rel.as_os_str().len())
        .max()
        .unwrap_or(6)
        .max(6);

    for e in &plan.entries {
        if changed_only && !e.state.needs_change() {
            continue;
        }
        let mark = match e.state {
            State::Linked => "\x1b[32mok\x1b[0m",
            State::Missing => "\x1b[33m--\x1b[0m",
            State::Wrong { .. } => "\x1b[33m~~\x1b[0m",
            State::Occupied => "\x1b[31m!!\x1b[0m",
        };
        println!(
            "{}  {:<width$}  -> {}",
            mark,
            e.rel.display(),
            e.dest.display(),
            width = width
        );
    }

    let n = plan.entries.len();
    let c = plan.changes().count();
    println!("\n{n} link(s), {c} need change");
}
