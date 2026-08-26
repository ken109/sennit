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
        /// Delete whatever is in the way instead of moving it aside.
        ///
        /// A directory is removed with everything under it.
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

    // compare は 2 つの JSON を読むだけなので、リポジトリの中に居る必要がない。
    // 別のマシンから受け取った報告を突き合わせるのが用途なので、ここで先に捌く。
    if let Command::Compare { a, b } = &cli.command {
        return verify::compare(a, b);
    }

    let root = match &cli.root {
        Some(p) => p.clone(),
        None => find_root().context("could not locate sennit.toml")?,
    };
    let home = match &cli.home {
        Some(p) => p.clone(),
        None => PathBuf::from(std::env::var("HOME").context("HOME is not set")?),
    };
    // 相対パスのまま symlink を張ると、リンク先が $HOME からの相対として
    // 解釈される。`sennit --root . apply` は .zshrc -> ./.zshrc という
    // 自分自身を指すリンクを作り、設定を全部壊す。絶対パスに直しておく。
    let root = absolute(&root).with_context(|| format!("bad --root: {}", root.display()))?;
    let home = absolute(&home).with_context(|| format!("bad --home: {}", home.display()))?;

    let manifest = Manifest::load(&root.join("sennit.toml"))?;
    let plan = Plan::build(&root, &home, &manifest)?;

    match cli.command {
        Command::Apply {
            dry_run,
            no_backup,
            secrets,
        } => {
            // 生成物はコミットしないので、配置の前に必ず作る
            let produced = render_all(&root, &manifest, secrets, dry_run)?;
            let plan = Plan::build(&root, &home, &manifest)?;
            apply(
                &plan, &root, &home, &manifest, dry_run, !no_backup, &produced,
            )
        }
        Command::Rollback { dry_run } => rollback(&home, dry_run),
        Command::Diff => {
            print_diff(&plan);
            Ok(())
        }
        Command::Check => check(&root),
        Command::Render { secrets } => render_all(&root, &manifest, secrets, false).map(|_| ()),
        Command::Sync { dry_run } => sync::sync(&root, dry_run),
        Command::Verify { export } => verify::verify(&root, export),
        // 先に捌いてある
        Command::Compare { .. } => unreachable!(),
        Command::Audit { history } => audit::audit(&root, history),
        Command::List { changed } => {
            print_list(&plan, changed);
            Ok(())
        }
    }
}

/// 相対パスを絶対パスに直す。存在すれば symlink も解決する。
///
/// 存在しない場合(初回の --home など)はカレントディレクトリを前置するだけに
/// して、「まだ無い」を失敗にしない。
fn absolute(p: &Path) -> Result<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Ok(c);
    }
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(p))
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
/// 生成物はコミットしない。apply が配置の前に必ず作るので clone 直後でも
/// 揃うし、同じ変更が差分に 2 度出るのを避けられる。秘密を含む生成物が
/// リポジトリに入らないのも同じ理由による。
/// 戻り値は「この実行で作られる(--dry-run なら作られるはずの)出力」。
/// apply はこれを見て、まだディスクに無いものを張ってよいか判断する。
fn render_all(
    root: &Path,
    manifest: &Manifest,
    secrets: bool,
    dry_run: bool,
) -> Result<std::collections::BTreeSet<PathBuf>> {
    let mut produced = decrypt_all(root, manifest, dry_run)?;
    if manifest.render.is_empty() {
        return Ok(produced);
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
        let raw = std::fs::read_to_string(&tmpl_path)
            .with_context(|| format!("failed to read template {}", tmpl_path.display()))?;
        // 条件を先に解決する。消える分岐の中の op:// は、このマシンでは
        // 要らない秘密なので数えない。
        let template = render::resolve_conditionals(&raw, &vars, tmpl_rel)?;

        // 秘密を参照するものは既定で飛ばす。1Password はサインインと
        // ロック解除を人手に要求するので、初回セットアップや CI、
        // コンテナでは原理的に成立しない。そこを異常扱いにしない。
        if !secrets && render::needs_secrets(&template) {
            deferred.push((out_rel.clone(), render::schemes_used(&template)));
            continue;
        }
        // --dry-run で秘密を取りに行かない。プロバイダの呼び出しは
        // 生体認証やロック解除を人に要求する。「何も起きない」と言った
        // コマンドがそれを出すのは筋が通らない。
        if dry_run && render::needs_secrets(&template) {
            println!("  \x1b[33mwould render\x1b[0m  {out_rel}  (reads secrets)");
            produced.insert(PathBuf::from(out_rel));
            continue;
        }
        produced.insert(PathBuf::from(out_rel));
        let rendered = render::expand_with(&template, &vars, tmpl_rel, &mut cache)?;

        // 中身が同じなら書き直さない。mtime が動くと apply が無駄に張り直す。
        // ただし権限だけは毎回揃える。中身が変わらない限り読み取り専用に
        // ならない、という穴があった。
        if std::fs::read_to_string(&out_path).ok().as_deref() == Some(rendered.as_str()) {
            if !dry_run {
                enforce_mode(
                    root,
                    &out_path,
                    manifest.mode_for(Path::new(out_rel)),
                    render::needs_secrets(&template),
                )?;
            }
            continue;
        }
        if dry_run {
            println!("  \x1b[33mwould render\x1b[0m  {out_rel}");
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mode =
            manifest
                .mode_for(Path::new(out_rel))
                .unwrap_or(if render::needs_secrets(&template) {
                    0o400
                } else {
                    0o444
                });
        write_generated(root, &out_path, rendered.as_bytes(), mode)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        println!("  \x1b[33mrendered\x1b[0m   {out_rel}");
    }

    if !deferred.is_empty() {
        // どのプロバイダが要るかはテンプレートが決める。プロバイダは宣言で
        // 足せるので、ここで 1Password と決め打ちにはできない。
        for (out_rel, schemes) in &deferred {
            let who = if schemes.is_empty() {
                String::from("needs a secret provider")
            } else {
                format!("needs {}", schemes.join(", "))
            };
            println!("  \x1b[36mdeferred\x1b[0m   {out_rel}  ({who})");
        }
        println!(
            "{} template(s) not rendered; run `sennit apply --secrets` once the provider is available",
            deferred.len()
        );
    }
    Ok(produced)
}

/// リポジトリ内の暗号文を復号して置く。
///
/// プロバイダと違い外部サービスもサインインも要らないので、鍵さえあれば
/// 無人で動く。鍵が無い環境では保留するが、それは「設定していない」であって
/// 「壊れている」ではない。
fn decrypt_all(
    root: &Path,
    manifest: &Manifest,
    dry_run: bool,
) -> Result<std::collections::BTreeSet<PathBuf>> {
    let mut produced = std::collections::BTreeSet::new();
    if manifest.encrypted.is_empty() {
        return Ok(produced);
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
        return Ok(produced);
    }

    for (out_rel, src_rel) in &manifest.encrypted {
        let src = root.join(src_rel);
        let out_path = root.join(out_rel);
        // 復号もコマンドの実行で、鍵によっては人手を要求する。--dry-run では走らせない。
        produced.insert(PathBuf::from(out_rel));
        if dry_run {
            println!("  \x1b[35mwould decrypt\x1b[0m  {out_rel}");
            continue;
        }
        let plain = enc.decrypt(&src)?;

        // 中身が同じでも権限は揃える
        if std::fs::read(&out_path).ok().as_deref() == Some(plain.as_slice()) {
            enforce_mode(root, &out_path, manifest.mode_for(Path::new(out_rel)), true)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 復号結果も生成物なので読み取り専用。暗号化してあった＝秘密なので 0400
        let mode = manifest.mode_for(Path::new(out_rel)).unwrap_or(0o400);
        write_generated(root, &out_path, &plain, mode)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        println!("  \x1b[35mdecrypted\x1b[0m  {out_rel}");
    }
    Ok(produced)
}

/// 生成物のあるべきモードに揃える。
///
/// 失敗を握り潰さない。秘密を含む生成物が誰でも読める状態のまま
/// コマンドが成功を返すと、権限で守るという前提そのものが崩れる。
fn enforce_mode(root: &Path, path: &Path, declared: Option<u32>, secret: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let want = declared.unwrap_or(if secret { 0o400 } else { 0o444 });
    must_be_inside(root, path)?;
    // 生成物の置き場が symlink なら、chmod はリンク先に掛かる
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.file_type().is_symlink() {
            bail!(
                "{} is a symlink; a generated file is written in place, not through a link",
                path.display()
            );
        }
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        // まだ無いものは次の書き込みで正しいモードになる
        Err(_) => return Ok(()),
    };
    if meta.permissions().mode() & 0o777 != want {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(want))
            .with_context(|| format!("failed to set mode on {}", path.display()))?;
    }
    Ok(())
}

/// 生成物を書く。
///
/// 既定で読み取り専用にする。生成物はテンプレートの隣に並ぶので、
/// うっかりそちらを開いて編集してしまう。書けてしまうと次の render で
/// 黙って消える。書けなくしておけばエディタが保存に失敗し、その場で気づく。
///
/// 中身を書く前に最終的なモードで作る。先に書いてから chmod すると、
/// umask 022 の既定で 0644 のファイルにトークンが入っている瞬間が生まれる。
/// このパスが本当にリポジトリの中を指しているか確かめる。
///
/// symlink_metadata が見るのは最後の 1 要素だけで、途中のディレクトリが
/// symlink なら素通りする。リポジトリに `out -> /somewhere/outside` を
/// 1 つ置くだけで、宣言の検査(`..` と絶対パスを断る)を回り込んで外の
/// ファイルを書いたり chmod したりできてしまう。
///
/// 書き込みも権限変更も、触る前にここを通す。
fn must_be_inside(root: &Path, path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or(root);
    let real = std::fs::canonicalize(parent)
        .with_context(|| format!("failed to resolve {}", parent.display()))?;
    let real_root = std::fs::canonicalize(root)
        .with_context(|| format!("failed to resolve {}", root.display()))?;
    if !real.starts_with(&real_root) {
        bail!(
            "{} resolves to {}, which is outside the repository",
            path.display(),
            real.display()
        );
    }
    Ok(())
}

fn write_generated(root: &Path, path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    must_be_inside(root, path)?;

    // 書き先を確かめてから触る。chmod も open も symlink を辿るので、
    // 「ディレクトリではない」だけ見て進むと、リンクの先にあるディレクトリを
    // 0600 にしてしまうし、リポジトリの外のファイルを黙って上書きできる。
    // 生成物の置き場が symlink である理由は無いので、どちらも断る。
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => bail!(
            "{} is a directory; a generated file cannot be written there",
            path.display()
        ),
        Ok(meta) if meta.file_type().is_symlink() => bail!(
            "{} is a symlink; a generated file is written in place, not through a link",
            path.display()
        ),
        // 前回書いたものが読み取り専用だと開けない
        Ok(_) => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
        }
        Err(_) => {}
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    f.write_all(contents)?;
    f.flush()?;
    drop(f);

    // 既存ファイルを開いた場合は .mode() が効かないので揃え直す
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// 宣言されたモードを揃える。戻り値は直した数。
///
/// 宣言されたパスそのものに掛ける。symlink 方式なのでリポジトリ側の実体が
/// 対象で、$HOME からはそれが見える。ファイルでもディレクトリでも同じ扱いに
/// する。verify が見るのも同じパスなので、両者が食い違わない。
///
/// preview では変えずに、掛けられるかだけ見る。締め出すような宣言を
/// --dry-run が通してしまうと、preview から始めろという案内が嘘になる。
fn enforce_modes(root: &Path, manifest: &Manifest, preview: bool) -> Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    let mut fixed = 0usize;
    for rel in manifest.modes.keys() {
        let want = manifest
            .mode_for(Path::new(rel))
            .expect("validated when the manifest loaded");
        let path = root.join(rel);
        // metadata も set_permissions も symlink を辿る。宣言されたパスが
        // リンクなら、chmod が掛かるのはリンク先 — リポジトリの外かもしれない。
        // 宣言は「そのパスそのもの」に効くと書いてあるので、リンクは断る。
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => bail!(
                "`{}` is a symlink; a declared mode applies to the path itself, \
                 and following the link would change something else",
                rel
            ),
            _ => {}
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            // まだ作られていない生成物。次に作られるときに正しいモードになる。
            //
            // 「無い」の判定はリポジトリの内外を見るより先。逆にすると、
            // 秘密を読むテンプレートの出力にモードを宣言しているだけで、
            // 親ディレクトリがまだ無い初回の apply が canonicalize に
            // 失敗して丸ごと落ちる。CI とコンテナは常にその状態になる。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("failed to stat {}", path.display()))
            }
        };
        // 途中のディレクトリが symlink なら、最後の 1 要素を見るだけでは
        // リポジトリの外に出ていることに気づけない。
        must_be_inside(root, &path)?;
        // ディレクトリは所有者の読みと実行の両方が要る。read_dir には読みが、
        // 中のファイルに触るには実行が必要で、どちらを落としても sennit は
        // 二度とそこを歩けない。しかも自分で掛けたモードなので、次の apply は
        // その下を「無くなった」と読み、verify はディレクトリ自身しか見ない。
        if meta.is_dir() && want & 0o500 != 0o500 {
            bail!(
                "mode {:o} on the directory `{}` would remove your own access to it",
                want,
                rel
            );
        }
        if meta.permissions().mode() & 0o777 == want {
            continue;
        }
        if preview {
            println!("  {:>8}  {}  ({:o})", "would set", rel, want);
            fixed += 1;
            continue;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(want))
            .with_context(|| format!("failed to set mode on {}", path.display()))?;
        // 落ちた先を読み直す。要求どおりにならないまま成功を返すと、
        // 毎回「直した」と言い続けて収束しない。
        let got = std::fs::metadata(&path)?.permissions().mode() & 0o777;
        if got != want {
            bail!(
                "{}: asked for mode {:o} but the filesystem gave {:o}",
                path.display(),
                want,
                got
            );
        }
        println!("  {:>8}  {}  ({:o})", "mode", rel, want);
        fixed += 1;
    }
    Ok(fixed)
}

fn apply(
    plan: &Plan,
    root: &Path,
    home: &Path,
    manifest: &Manifest,
    dry_run: bool,
    backup: bool,
    // この実行で作られる生成物。--dry-run では実際には書かれていないので、
    // 「まだ無い」と「これから作られる」を取り違えないために要る。
    produced: &std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    let previous = state::State::load(home)?;

    // 「張れる状態にあるか」を 1 か所で決める。
    //
    // Path::exists() は使えない。あれは EACCES も「無い」と答えるので、
    // 読めなくなっただけのファイルが記録から落ち、次の apply がそれを
    // 「宣言から外れた」と読んで $HOME のリンクを消す。無いのか読めないのか
    // は区別しなければならない。
    let placeable = |e: &plan::Entry| -> Result<bool> {
        match std::fs::symlink_metadata(&e.src) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // --dry-run では生成物がまだ書かれていない。これから作られる
                // ものを「置けない」と出すと、preview が本番と食い違う。
                Ok(dry_run && produced.contains(&e.rel))
            }
            Err(err) => Err(anyhow::Error::new(err))
                .with_context(|| format!("failed to read {}", e.src.display())),
        }
    };

    // 記録に載せるのは実際に張ったものだけ。まだ作られていない生成物を
    // 載せると、次の apply がそれを stale と見なして消しにかかる。
    let mut current: Vec<PathBuf> = Vec::new();
    for e in &plan.entries {
        if placeable(e)? {
            current.push(e.rel.clone());
        }
    }
    let stale = previous.stale(&current);

    // 宣言されている生成物は、まだ作られていないことがある。秘密を読む
    // テンプレートは --secrets を渡すまで飛ばされるので、その状態で張ると
    // 行き先の無い symlink が $HOME に残る。数には出すが、張らない。
    let mut changes: Vec<&plan::Entry> = Vec::new();
    let mut not_yet: Vec<&plan::Entry> = Vec::new();
    for e in plan.changes() {
        if placeable(e)? {
            changes.push(e);
        } else {
            not_yet.push(e);
        }
    }

    // 退避の記録は積み上げる。次の apply が上書きすると、前回退避した
    // 利用者のファイルが .sennit-backup のまま行き場を失う。
    let mut kept: Vec<state::Backup> = previous
        .backups
        .iter()
        .filter(|b| b.kept_at.exists())
        .cloned()
        .collect();
    let mut pruned = 0usize;

    // 途中で落ちても記録できるよう、張れたものを順に積む。既に正しく
    // 張られていたものが出発点。
    let mut placed: Vec<PathBuf> = current
        .iter()
        .filter(|rel| !plan.changes().any(|e| &&e.rel == rel))
        .cloned()
        .collect();

    // 前回張ったが今回の宣言から外れたもの。放っておくと管理をやめた設定の
    // リンクが $HOME に残り続ける。
    //
    // 消してよいのは自分が張ったリンクだけ。管理をやめた後に利用者が
    // 別の場所を指すリンクを自分で張り直していることがあり、それを
    // 「前回の記録に載っている」というだけで消すのは越権になる。
    for rel in &stale {
        let dest = home.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&dest) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        match std::fs::read_link(&dest) {
            Ok(target) if target.starts_with(root) => {}
            _ => {
                println!(
                    "  {:>8}  {}  (points outside the repository; left alone)",
                    "keep",
                    rel.display()
                );
                continue;
            }
        }
        println!("  {:>8}  {}", "prune", rel.display());
        pruned += 1;
        if !dry_run {
            std::fs::remove_file(&dest)
                .with_context(|| format!("failed to remove {}", dest.display()))?;
        }
    }

    for e in &not_yet {
        println!("  {:>8}  {}  (not generated yet)", "defer", e.rel.display());
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

        let step = (|| -> Result<()> {
            if let Some(parent) = e.dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            if let Some(at) = remove_dest(&e.dest, &e.state, backup)? {
                kept.push(state::Backup {
                    dest: e.dest.clone(),
                    kept_at: at,
                });
                // 退避したその場で記録する。以降のどこかで落ちると、
                // 動かしたファイルの在り処だけが分からなくなる。
                state::State {
                    links: placed.clone(),
                    backups: kept.clone(),
                    hooks: previous.hooks.clone(),
                }
                .save(home)?;
            }
            std::os::unix::fs::symlink(&e.src, &e.dest)
                .with_context(|| format!("failed to link {}", e.dest.display()))?;
            Ok(())
        })();

        match step {
            Ok(()) => placed.push(e.rel.clone()),
            Err(err) => {
                // ここまでに張ったものは $HOME に在る。記録せずに抜けると
                // 次の apply がそれを知らず、宣言から外しても prune できない
                // ——誰も管理していないリンクが残り続ける。
                state::State {
                    links: placed.clone(),
                    backups: kept.clone(),
                    hooks: previous.hooks.clone(),
                }
                .save(home)?;
                return Err(err);
            }
        }
    }

    if dry_run {
        // フックは実際には走らせず、何が走るかだけ出す
        hooks::run_all(root, &manifest.hooks, &previous.hooks, true)?;
        // モードも実際には変えないが、掛けられるかは見ておく。ディレクトリを
        // 締め出すような宣言をここで断らないと、preview だけ通って apply が
        // 落ちる。README は diff と --dry-run から始めろと書いてある。
        enforce_modes(root, manifest, true)?;
        println!(
            "\n{} change(s), {} prune(s), nothing written (--dry-run)",
            changes.len() + not_yet.len(),
            pruned
        );
        return Ok(());
    }

    // 張った直後に記録を確定させる。モードもフックもこのあとで落ちうるが、
    // リンクはもう $HOME に在る。記録しないまま抜けると、次の apply が
    // それを知らないので prune の対象にもならず、管理から外れたリンクが
    // 残り続ける。記録が先、後始末はあと。
    state::State {
        links: current.clone(),
        backups: kept.clone(),
        hooks: previous.hooks.clone(),
    }
    .save(home)?;

    // 宣言されたモードを揃える。symlink 方式なのでリポジトリ側の実体に
    // かける。生成物は render 側で既に揃っているが、ただ張っただけの
    // ファイルはここでしか直せない。
    let fixed = enforce_modes(root, manifest, false)?;

    // 配置のあとにフックを走らせる。設定を置いてから取り込む処理なので、
    // 順序が逆だと参照先がまだ無い。
    //
    // 監視対象の無いフックは毎回走るので指紋が動かない。指紋の比較だけでは
    // 「何かした」を取りこぼし、実際にコマンドを走らせた回に「最新です」と
    // 出てしまう。走った本数を見る。
    let ran = hooks::run_all(root, &manifest.hooks, &previous.hooks, false)?;
    let hooks = ran.fingerprints;

    // 記録は毎回書く。「変更なし」で書かずに抜けると、手で張られた環境や
    // state を消した環境で links が空のままになり、prune が永久に効かない。
    state::State {
        links: current,
        backups: kept.clone(),
        hooks: hooks.clone(),
    }
    .save(home)?;

    if changes.is_empty() && pruned == 0 && fixed == 0 && ran.count == 0 {
        if not_yet.is_empty() {
            println!("already up to date ({} links)", plan.entries.len());
        } else {
            // 置いていないものがあるのに「最新です」と言わない
            println!(
                "{} link(s) in place, {} not generated yet",
                plan.entries.len() - not_yet.len(),
                not_yet.len()
            );
        }
        return Ok(());
    }

    println!("\n{} link(s) updated, {} pruned", changes.len(), pruned);
    if !not_yet.is_empty() {
        println!("{} not generated yet, so not linked", not_yet.len());
    }
    if !kept.is_empty() {
        println!(
            "{} file(s) moved aside; `sennit rollback` puts them back",
            kept.len()
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

    // 同じ行き先に複数の退避があると、順に rename して最後のものだけが
    // 残る。つまり最も古い — 利用者が sennit を入れる前から持っていた —
    // ファイルを、戻す動作そのものが消していた。
    //
    // 戻すのは行き先ごとに最後の 1 つだけ。古い方はファイルとして残し、
    // どこに在るかを伝える。消すよりは残す。
    let mut newest: Vec<&state::Backup> = Vec::new();
    let mut shadowed: Vec<&state::Backup> = Vec::new();
    for b in st.backups.iter().rev() {
        if newest.iter().any(|n| n.dest == b.dest) {
            shadowed.push(b);
        } else {
            newest.push(b);
        }
    }
    newest.reverse();

    let mut restored = 0usize;
    let mut missing = 0usize;
    for b in &newest {
        println!("  {:>8}  {}", "restore", b.dest.display());
        // 退避そのものが消えていることがある。数に入れると「戻した」と
        // 言いながら何も戻していない状態になる。
        if !b.kept_at.exists() {
            println!("            the backup is gone; nothing to put back");
            missing += 1;
            continue;
        }
        if dry_run {
            restored += 1;
            continue;
        }
        // 張った symlink を外してから書き戻す。
        //
        // symlink でない実体が居る場合は、apply の後に利用者が書いたもの。
        // その上に rename すると黙って消える。退避を戻すコマンドが、
        // 戻す先にあったものを失わせるのでは意味が無いので、そちらも退ける。
        match std::fs::symlink_metadata(&b.dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                std::fs::remove_file(&b.dest)?;
            }
            Ok(_) => {
                let aside = backup_path(&b.dest)?;
                std::fs::rename(&b.dest, &aside)
                    .with_context(|| format!("failed to move {} aside", b.dest.display()))?;
                println!("            what was there is now at {}", aside.display());
            }
            Err(_) => {}
        }
        std::fs::rename(&b.kept_at, &b.dest)
            .with_context(|| format!("failed to restore {}", b.dest.display()))?;
        restored += 1;
    }

    for b in shadowed.iter().rev() {
        println!(
            "  {:>8}  {}  (an older copy of {}; left where it is)",
            "kept",
            b.kept_at.display(),
            b.dest.display()
        );
    }

    if dry_run {
        println!("\n{restored} file(s) would be restored (--dry-run)");
        if missing > 0 {
            println!("{missing} recorded backup(s) are no longer on disk");
        }
        return Ok(());
    }
    let n = restored;
    let older = shadowed.len();

    // 記録は空にする。古い退避を残すと、次の rollback がそれを今戻した
    // ファイルの上に書いてしまう。3 つあれば 2 つが消える。rollback は
    // 何度打っても同じ結果でなければならない。
    //
    // ファイルは消さない。記録から外れるだけで、場所は上に出してある。
    st.backups.clear();
    st.save(home)?;
    println!("\n{n} file(s) restored");
    if missing > 0 {
        // 記録は消す。戻せないものを残しても次の rollback が同じことを言う。
        println!("{missing} recorded backup(s) were already gone; nothing was put back for them");
    }
    if older > 0 {
        println!("{older} older copy(ies) left on disk; move them back by hand if you want them");
    }
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
                // ディレクトリだと消えるのは 1 ファイルではなく木ごと。
                // --no-backup を渡した本人にも、何が消えるかは見えていない。
                let n = walkdir::WalkDir::new(dest)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                    .count();
                println!("            deleting the directory and its {n} file(s)");
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
            State::Occupied => {
                let what = match std::fs::symlink_metadata(&e.dest) {
                    Ok(m) if m.is_dir() => "a directory",
                    _ => "a real file",
                };
                println!(
                    "\x1b[31m!\x1b[0m {}  ({what} is in the way; would be moved aside)",
                    e.rel.display()
                )
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 使い捨ての作業ディレクトリ。名前で衝突しないようにする。
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sennit-main-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_relative_root_becomes_absolute() {
        // 相対のままだと .zshrc -> ./.zshrc という自分を指すリンクになる
        let p = absolute(Path::new(".")).unwrap();
        assert!(p.is_absolute(), "{}", p.display());
    }

    #[test]
    fn a_path_that_does_not_exist_yet_is_still_made_absolute() {
        let p = absolute(Path::new("no/such/dir")).unwrap();
        assert!(p.is_absolute());
        assert!(p.ends_with("no/such/dir"));
    }

    #[test]
    fn a_backup_does_not_overwrite_an_earlier_backup() {
        let d = scratch("backup-path");
        let dest = d.join("a.conf");
        assert_eq!(backup_path(&dest).unwrap(), d.join("a.conf.sennit-backup"));

        std::fs::write(d.join("a.conf.sennit-backup"), "first").unwrap();
        assert_eq!(
            backup_path(&dest).unwrap(),
            d.join("a.conf.sennit-backup.1")
        );

        std::fs::write(d.join("a.conf.sennit-backup.1"), "second").unwrap();
        assert_eq!(
            backup_path(&dest).unwrap(),
            d.join("a.conf.sennit-backup.2")
        );
    }

    #[test]
    fn a_real_file_in_the_way_is_moved_aside_not_deleted() {
        let d = scratch("remove-dest-backup");
        let dest = d.join("a.conf");
        std::fs::write(&dest, "MINE").unwrap();

        let kept = remove_dest(&dest, &State::Occupied, true).unwrap().unwrap();
        assert!(!dest.exists());
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "MINE");
    }

    #[test]
    fn a_symlink_pointing_elsewhere_is_removed_without_a_backup() {
        let d = scratch("remove-dest-wrong");
        let other = d.join("other");
        std::fs::write(&other, "x").unwrap();
        let dest = d.join("a.conf");
        std::os::unix::fs::symlink(&other, &dest).unwrap();

        let kept = remove_dest(
            &dest,
            &State::Wrong {
                current: other.clone(),
            },
            true,
        )
        .unwrap();
        // リンクそのものに中身は無いので退避する意味がない
        assert!(kept.is_none());
        assert!(!dest.exists());
        // 指していた実体は残す
        assert!(other.exists());
    }

    #[test]
    fn no_backup_deletes_a_whole_directory() {
        let d = scratch("remove-dest-dir");
        let dest = d.join("nvim");
        std::fs::create_dir_all(dest.join("lua")).unwrap();
        std::fs::write(dest.join("init.lua"), "x").unwrap();
        std::fs::write(dest.join("lua/plugins.lua"), "x").unwrap();

        remove_dest(&dest, &State::Occupied, false).unwrap();
        assert!(!dest.exists());
    }

    #[test]
    fn a_directory_in_the_way_is_moved_aside_whole() {
        let d = scratch("remove-dest-dir-backup");
        let dest = d.join("nvim");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("init.lua"), "MINE").unwrap();

        let kept = remove_dest(&dest, &State::Occupied, true).unwrap().unwrap();
        assert!(!dest.exists());
        assert_eq!(
            std::fs::read_to_string(kept.join("init.lua")).unwrap(),
            "MINE"
        );
    }

    #[test]
    fn a_generated_file_is_created_with_its_final_mode() {
        let d = scratch("write-generated");
        let p = d.join("out.conf");
        write_generated(&d, &p, b"token", 0o400).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "token");
    }

    #[test]
    fn a_read_only_generated_file_can_still_be_rewritten() {
        let d = scratch("write-generated-again");
        let p = d.join("out.conf");
        write_generated(&d, &p, b"one", 0o444).unwrap();
        // 読み取り専用にしたものを次の render が書き直せないと詰む
        write_generated(&d, &p, b"two", 0o444).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    #[test]
    fn enforce_mode_fixes_a_file_left_at_the_wrong_mode() {
        let d = scratch("enforce-mode");
        let p = d.join("out.conf");
        std::fs::write(&p, "x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        enforce_mode(&d, &p, None, true).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }
}
