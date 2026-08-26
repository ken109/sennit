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
    /// リポジトリルート(既定: sennit.toml を上方向に探索)
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// 配置先(既定: $HOME)
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// symlink を配置する
    Apply {
        /// 実際には変更せず、何をするかだけ表示する
        #[arg(long)]
        dry_run: bool,
        /// symlink でない実体を、退避せずに削除する
        #[arg(long)]
        no_backup: bool,
        /// 1Password を参照するテンプレートも展開する
        #[arg(long)]
        secrets: bool,
    },
    /// 適用したときに何が変わるかを表示する
    Diff,
    /// 設定が参照している依存が packages.toml に宣言されているか検証する
    Check,
    /// シェル履歴を見て、宣言したコマンドが実際に使われているか棚卸しする
    Audit {
        /// 履歴ファイル(既定: ~/.zsh_history か ~/.bash_history)
        #[arg(long)]
        history: Option<PathBuf>,
    },
    /// 宣言したものがこのマシンで実際に解決できるか確かめる
    Verify {
        /// 結果を JSON で書き出す(マシン間の比較用)
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// 2 台分の verify --export を比べる
    Compare { a: PathBuf, b: PathBuf },
    /// packages.toml の宣言をもとに未導入のパッケージを入れる
    Sync {
        /// 実際には入れず、何を入れるかだけ表示する
        #[arg(long)]
        dry_run: bool,
    },
    /// theme.toml からテンプレートを展開して設定ファイルを生成する
    Render {
        /// 1Password を参照するテンプレートも展開する。
        /// 既定では飛ばす。op はログインとロック解除を人手に要求するので、
        /// 自動セットアップの途中では必ず失敗するため。
        #[arg(long)]
        secrets: bool,
    },
    /// 直前の apply が退避したファイルを元に戻す
    Rollback {
        /// 実際には戻さず、何を戻すかだけ表示する
        #[arg(long)]
        dry_run: bool,
    },
    /// 配置状況を一覧する
    List {
        /// 差分のあるものだけ表示する
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

        // 中身が同じなら触らない。mtime が動くと apply が無駄に張り直す
        if std::fs::read_to_string(&out_path).ok().as_deref() == Some(rendered.as_str()) {
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

        // 中身が同じなら触らない
        if std::fs::read(&out_path).ok().as_deref() == Some(plain.as_slice()) {
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&out_path, &plain)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        // 暗号化してあるということは秘密なので、既定で 0600
        let mode = manifest.mode_for(Path::new(out_rel)).unwrap_or(0o600);
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
fn write_rendered(path: &Path, contents: &str, secret: bool, declared: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents)?;
    // 宣言があればそれに従う。無くても秘密を含むなら 0600 にする。
    // 既定の umask 022 では 0644 になり、トークンが誰でも読める。
    if let Some(mode) = declared.or(if secret { Some(0o600) } else { None }) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
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
