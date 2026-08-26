use crate::packages::Kind;
use anyhow::Result;
use std::path::Path;

/// 設定ファイルが要求している外部依存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub kind: Kind,
    pub name: String,
    /// どのファイルのどこから読み取ったか
    pub source: String,
}

/// リポジトリ全体を走査して要求を集める。
///
/// 検出器は設定フォーマットごとに書く。汎用的な文字列検索にすると
/// 誤検出だらけになるため、意味の分かっている箇所だけを見る。
pub fn scan(root: &Path) -> Result<Vec<Requirement>> {
    let mut reqs = Vec::new();
    git_config(root, &mut reqs)?;
    alacritty(root, &mut reqs)?;
    zed(root, &mut reqs)?;
    zsh(root, &mut reqs)?;
    config_dirs(root, &mut reqs)?;
    annotations(root, &mut reqs)?;
    secret_templates(root, &mut reqs)?;
    encryption_tool(root, &mut reqs)?;
    reqs.sort_by(|a, b| (a.kind.label(), &a.name).cmp(&(b.kind.label(), &b.name)));
    reqs.dedup_by(|a, b| a.kind == b.kind && a.name == b.name);
    Ok(reqs)
}

/// 設定ファイルを読む。生成物がまだ無ければテンプレートを読む。
///
/// 生成物はコミットしない方針なので、clone 直後や CI では実体が存在しない。
/// テンプレートは置換前でも、フォント名のような値はそのまま書かれている。
fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel))
        .or_else(|_| std::fs::read_to_string(root.join(format!("{rel}.tmpl"))))
        .ok()
}

/// 先頭の `!`(git のシェル実行記法)と引数を落として、実行されるコマンド名を取る。
fn command_name(value: &str) -> Option<String> {
    let v = value.trim().trim_start_matches('!').trim();
    let first = v.split_whitespace().next()?;
    // 絶対パス指定でも実体名を取る
    let name = first.rsplit('/').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// git config: pager / diffFilter / credential helper が外部コマンドを呼ぶ。
fn git_config(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Some(text) = read(root, ".config/git/config") else {
        return Ok(());
    };
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !matches!(key, "pager" | "diffFilter" | "helper") {
            continue;
        }
        if let Some(name) = command_name(value) {
            out.push(Requirement {
                kind: Kind::Command,
                name,
                source: format!(".config/git/config ({key})"),
            });
        }
    }
    Ok(())
}

/// alacritty: フォントファミリ
fn alacritty(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Some(text) = read(root, ".config/alacritty/alacritty.toml") else {
        return Ok(());
    };
    for cap in find_all(&text, "family = \"") {
        out.push(Requirement {
            kind: Kind::Font,
            name: cap,
            source: ".config/alacritty/alacritty.toml (font.family)".into(),
        });
    }
    Ok(())
}

/// zed: フォントファミリと、auto_install_extensions が要求する拡張
fn zed(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Some(text) = read(root, ".config/zed/settings.json") else {
        return Ok(());
    };
    for key in ["\"buffer_font_family\": \"", "\"font_family\": \""] {
        for cap in find_all(&text, key) {
            out.push(Requirement {
                kind: Kind::Font,
                name: cap,
                source: ".config/zed/settings.json (font_family)".into(),
            });
        }
    }
    // auto_install_extensions のブロック内のキーを拾う
    if let Some(start) = text.find("\"auto_install_extensions\"") {
        if let Some(open) = text[start..].find('{') {
            let rest = &text[start + open..];
            if let Some(close) = rest.find('}') {
                for cap in find_all(&rest[..close], "\"") {
                    if !cap.is_empty() {
                        out.push(Requirement {
                            kind: Kind::Extension,
                            name: cap,
                            source: ".config/zed/settings.json (auto_install_extensions)".into(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// zsh: eval "$(cmd ...)" と ${+commands[cmd]} が実在を前提にしているコマンド
fn zsh(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let mut files: Vec<std::path::PathBuf> = vec![root.join(".zshenv")];
    if let Ok(dir) = std::fs::read_dir(root.join(".config/zsh/rc")) {
        files.extend(dir.filter_map(|e| e.ok()).map(|e| e.path()));
    }
    files.push(root.join(".config/zsh/.zshrc"));

    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();

        for cap in find_all(&text, "eval \"$(") {
            if let Some(name) = command_name(&cap) {
                out.push(Requirement {
                    kind: Kind::Command,
                    name,
                    source: format!("{rel} (eval)"),
                });
            }
        }
        for cap in find_all(&text, "${+commands[") {
            let name = cap.trim_end_matches(']').to_string();
            if !name.is_empty() {
                out.push(Requirement {
                    kind: Kind::Command,
                    name,
                    source: format!("{rel} (commands[])"),
                });
            }
        }
    }
    Ok(())
}

/// 秘密を参照するテンプレートは、そのプロバイダのコマンドを要求する。
///
/// テンプレートに書いた瞬間に依存が生まれるが、それはどの設定ファイルにも
/// 現れない。宣言し忘れると、秘密を使うマシンでだけ失敗する。
///
/// どのコマンドが要るかは scheme ごとに違う。`{{ pass://... }}` しか
/// 使っていない repo に op を要求するのは、プロバイダを宣言で足せる
/// という設計と噛み合わない。
fn secret_templates(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    // 宣言があればそこから、無ければ既定の op から、scheme -> コマンド名を引く
    let providers = match crate::manifest::Manifest::load(&root.join("sennit.toml")) {
        Ok(m) if !m.providers.is_empty() => m.providers,
        _ => crate::render::default_providers(),
    };
    // テンプレートは .config の外にも置ける(.npmrc.tmpl)。リポジトリ全体から
    // 拡張子で拾う。
    let mut files = Vec::new();
    walk(root, &mut files, 0);

    for path in files {
        if path.extension().is_none_or(|e| e != "tmpl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for scheme in crate::render::schemes_used(&text) {
            let Some(provider) = providers.get(&scheme) else {
                // 宣言の無い scheme は render が名指しで落とす。ここでは黙る。
                continue;
            };
            let Some(bin) = crate::render::shell_words(&provider.command)
                .into_iter()
                .next()
            else {
                continue;
            };
            out.push(Requirement {
                kind: Kind::Command,
                name: bin,
                source: format!(
                    "{} ({scheme}:// reference)",
                    path.strip_prefix(root).unwrap_or(&path).display()
                ),
            });
        }
    }
    Ok(())
}

/// [encryption] を宣言しているなら、その復号コマンドが要る。
///
/// テンプレートの op:// と同じで、宣言した瞬間に依存が生まれるが
/// どの設定ファイルにも現れない。
fn encryption_tool(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let manifest_path = root.join("sennit.toml");
    let Ok(manifest) = crate::manifest::Manifest::load(&manifest_path) else {
        return Ok(());
    };
    let Some(enc) = manifest.encryption else {
        return Ok(());
    };
    let Some(bin) = crate::render::shell_words(&enc.command).into_iter().next() else {
        return Ok(());
    };
    let name = bin.rsplit('/').next().unwrap_or(&bin).to_string();
    out.push(Requirement {
        kind: Kind::Command,
        name,
        source: "sennit.toml ([encryption])".into(),
    });
    Ok(())
}

/// 設定ファイル自身に書かれた宣言を読む。
///
/// 検出器はフォーマットごとに手書きなので、新しい種類の設定が増えるたびに
/// コードが要る。実際 *.tmpl と .config/theme/ で 2 回穴が開いた。
/// 設定の側が自分の依存を書けるなら、パースも対応も要らない。
///
///     # sennit: requires command hunk
///     # sennit: requires font "Hack Nerd Font Mono"
///
/// コメント記号は問わない。行のどこかにこの並びがあればよい。
fn annotations(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let mut files = Vec::new();
    walk(&root.join(".config"), &mut files, 0);
    files.push(root.join(".zshenv"));

    for path in files {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() > 512 * 1024 {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();

        for line in text.lines() {
            let Some(idx) = line.find("sennit: requires ") else {
                continue;
            };
            let rest = line[idx + "sennit: requires ".len()..].trim();
            let (kind, value) = match rest.split_once(char::is_whitespace) {
                Some(("command", v)) => (Kind::Command, v),
                Some(("font", v)) => (Kind::Font, v),
                Some(("extension", v)) => (Kind::Extension, v),
                _ => continue,
            };
            let value = value.trim().trim_matches('"').trim();
            if value.is_empty() {
                continue;
            }
            out.push(Requirement {
                kind,
                name: value.to_string(),
                source: format!("{rel} (annotation)"),
            });
        }
    }
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        if p.is_dir() {
            walk(&p, out, depth + 1);
        } else {
            out.push(p);
        }
    }
}

/// .config/<name> の存在そのものが、そのツールへの依存を表す。
///
/// direnv のように「設定ディレクトリはあるが、どの設定ファイルからも
/// 名前が参照されない」依存はこれでしか捕まらない。実際 direnvrc を配置
/// しているのに direnv を入れていない期間があった。
/// 対象外にしたいものは packages.toml の ignore.commands に書く。
fn config_dirs(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Ok(dir) = std::fs::read_dir(root.join(".config")) else {
        return Ok(());
    };
    for entry in dir.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        // テンプレートは生成元。生成物があるならそちらで検出されるので飛ばすが、
        // 生成物がまだ無ければ .tmpl を剥がした名前で拾う。
        let name = match name.strip_suffix(".tmpl") {
            Some(base) => {
                if entry.path().with_file_name(base).exists() {
                    continue;
                }
                base.to_string()
            }
            None => name,
        };
        // starship.toml のような単一ファイル形式も拾う。拡張子は .toml とは
        // 限らない(kitty.conf, foo.yaml)。.toml だけ剥がすと、どのパッケージ
        // 名とも一致しない "kitty.conf" が要求として出て check が落ちる。
        let name = if entry.path().is_dir() {
            name
        } else {
            Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(name)
        };
        if name.starts_with('.') || name.is_empty() {
            continue;
        }
        out.push(Requirement {
            kind: Kind::Command,
            name,
            source: ".config/ (directory exists)".into(),
        });
    }
    Ok(())
}

/// prefix に続く、閉じ記号までの文字列をすべて拾う小さなヘルパ。
/// prefix の末尾文字を終端記号とみなす(`"` なら `"`、`(` なら `)`、`[` なら `]`)。
fn find_all(text: &str, prefix: &str) -> Vec<String> {
    let close = match prefix.chars().last() {
        Some('"') => '"',
        Some('(') => ')',
        Some('[') => ']',
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(prefix) {
        let after = &rest[i + prefix.len()..];
        if let Some(j) = after.find(close) {
            out.push(after[..j].to_string());
            rest = &after[j..];
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 検出器を通すための最小のリポジトリを組む。
    struct Fixture {
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("sennit-detect-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(".config")).unwrap();
            std::fs::write(root.join("sennit.toml"), "[link]\ncommon = []\n").unwrap();
            Fixture { root }
        }

        fn file(&self, rel: &str, body: &str) -> &Self {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
            self
        }

        fn dir(&self, rel: &str) -> &Self {
            std::fs::create_dir_all(self.root.join(rel)).unwrap();
            self
        }

        fn manifest(&self, body: &str) -> &Self {
            std::fs::write(self.root.join("sennit.toml"), body).unwrap();
            self
        }

        fn names(&self) -> Vec<String> {
            scan(&self.root)
                .unwrap()
                .into_iter()
                .map(|r| r.name)
                .collect()
        }
    }

    #[test]
    fn a_config_directory_is_evidence_of_a_dependency() {
        let f = Fixture::new("config-dir");
        f.dir(".config/direnv");
        assert!(f.names().contains(&"direnv".to_string()));
    }

    /// starship.toml のような単一ファイル形式。拡張子は .toml とは限らない。
    #[test]
    fn a_single_config_file_is_named_without_its_extension() {
        let f = Fixture::new("config-file");
        f.file(".config/starship.toml", "");
        f.file(".config/kitty.conf", "");
        f.file(".config/some.yaml", "");
        let names = f.names();
        for want in ["starship", "kitty", "some"] {
            assert!(
                names.contains(&want.to_string()),
                "{names:?} missing {want}"
            );
        }
        // 拡張子付きのままの名前は、どのパッケージとも一致しない
        for bad in ["kitty.conf", "some.yaml"] {
            assert!(!names.contains(&bad.to_string()), "{names:?} has {bad}");
        }
    }

    /// 生成物があるならテンプレートは数えない。
    #[test]
    fn a_template_is_skipped_when_its_output_exists() {
        let f = Fixture::new("tmpl-skip");
        f.dir(".config/alacritty.tmpl");
        f.dir(".config/alacritty");
        let names = f.names();
        assert_eq!(names.iter().filter(|n| *n == "alacritty").count(), 1);
    }

    /// 秘密は、その scheme を宣言したプロバイダのコマンドを要求する。
    #[test]
    fn a_secret_requires_the_provider_it_actually_names() {
        let f = Fixture::new("provider");
        f.manifest("[link]\ncommon = []\n\n[providers.pass]\ncommand = \"pass show {}\"\n");
        f.file(".config/x.conf.tmpl", "token = {{ pass://a/b }}\n");
        let names = f.names();
        assert!(names.contains(&"pass".to_string()), "{names:?}");
        // 宣言していない op を要求しない
        assert!(!names.contains(&"op".to_string()), "{names:?}");
    }

    /// 宣言が無ければ既定の op。
    #[test]
    fn without_a_declaration_a_secret_requires_op() {
        let f = Fixture::new("provider-default");
        f.file(".config/x.conf.tmpl", "token = {{ op://a/b }}\n");
        assert!(f.names().contains(&"op".to_string()));
    }

    #[test]
    fn an_annotation_declares_a_requirement_by_hand() {
        let f = Fixture::new("annotation");
        f.file(".config/x/conf", "# sennit: requires command jq\n");
        assert!(f.names().contains(&"jq".to_string()));
    }

    #[test]
    fn a_font_family_in_the_terminal_config_is_a_requirement() {
        let f = Fixture::new("font");
        f.file(
            ".config/alacritty/alacritty.toml",
            "[font.normal]\nfamily = \"Hack Nerd Font Mono\"\n",
        );
        let reqs = scan(&f.root).unwrap();
        assert!(
            reqs.iter()
                .any(|r| r.kind == Kind::Font && r.name == "Hack Nerd Font Mono"),
            "{reqs:?}"
        );
    }

    /// git の設定が呼ぶコマンド。`!` のシェル前置と絶対パスを剥がす。
    #[test]
    fn a_command_from_git_config_is_a_requirement() {
        let f = Fixture::new("git");
        f.file(
            ".config/git/config",
            "[core]\n\tpager = delta\n[interactive]\n\tdiffFilter = delta --color-only\n",
        );
        assert!(f.names().contains(&"delta".to_string()));
    }

    /// 隠しファイルは拾わない。
    #[test]
    fn dotfiles_under_config_are_not_requirements() {
        let f = Fixture::new("hidden");
        f.file(".config/.DS_Store", "");
        assert!(!f.names().contains(&".DS_Store".to_string()));
    }
}
