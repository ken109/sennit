use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

/// テンプレートから見える値をまとめる。
///
/// データファイル(既定は theme.toml)に加えて、その場でしか分からない値も
/// 入れる。ホスト名やプロファイルは配色と違ってファイルに書けないが、
/// マシンごとに変える設定では最も必要になる。
pub fn load_vars(paths: &[std::path::PathBuf]) -> Result<BTreeMap<String, String>> {
    let mut vars = BTreeMap::new();

    for path in paths {
        // 宣言されたデータファイルが無いのは設定漏れなので黙って進まない
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value: toml::Value =
            toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        flatten(&value, String::new(), &mut vars);
    }

    vars.insert("sennit.os".into(), crate::packages::current_os().into());
    vars.insert("sennit.hostname".into(), hostname());
    vars.insert(
        "sennit.profile".into(),
        crate::packages::current_profiles().join(","),
    );
    for (k, v) in std::env::vars() {
        vars.insert(format!("env.{k}"), v);
    }
    Ok(vars)
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn flatten(value: &toml::Value, prefix: String, out: &mut BTreeMap<String, String>) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(v, key, out);
            }
        }
        toml::Value::String(s) => {
            out.insert(prefix, s.clone());
        }
        other => {
            out.insert(prefix, other.to_string());
        }
    }
}

/// `scheme://rest` の形なら分解する。テンプレート変数は `ui.bg` のような
/// ドット記法なので、`://` の有無で区別できる。
pub fn split_reference(key: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = key.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() || scheme.contains(char::is_whitespace) {
        return None;
    }
    Some((scheme, rest))
}

/// このテンプレートが秘密を参照しているか。
///
/// 宣言させるのではなく中身から判定する。書き忘れると初回セットアップが
/// 落ちる種類の宣言は、そもそも人間に書かせない方がよい。
pub fn needs_secrets(template: &str) -> bool {
    let mut rest = template;
    while let Some(i) = rest.find("{{") {
        let after = &rest[i + 2..];
        let Some(end) = after.find("}}") else {
            return false;
        };
        if split_reference(after[..end].trim()).is_some() {
            return true;
        }
        rest = &after[end..];
    }
    false
}

/// このテンプレートが参照している秘密のスキーム。重複は畳む。
///
/// 保留したときに「何が要るのか」を出すために使う。プロバイダは宣言で
/// 足せるので、名前を決め打ちにはできない。
pub fn schemes_used(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find("{{") {
        let after = &rest[i + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        if let Some((scheme, _)) = split_reference(after[..end].trim()) {
            if !out.iter().any(|s| s == scheme) {
                out.push(scheme.to_string());
            }
        }
        rest = &after[end..];
    }
    out
}

/// `{{ key }}` を差し替えるだけの最小のテンプレート展開。
///
/// 汎用テンプレートエンジンを入れないのは、条件分岐やループを持ち込むと
/// 生成元が「設定ファイルとして読めるもの」でなくなるため。置換だけに
/// 限れば *.tmpl は元の設定とほぼ同じ見た目のまま保てる。
/// 秘密の取り出し方。
///
/// 主要なプロバイダはどれも「コマンドを実行して標準出力を受け取る」形なので、
/// プロバイダごとに実装を書かない。scheme とコマンドの対応を宣言してもらう。
/// こうすると sennit が知らないプロバイダでも動く。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Provider {
    /// `{}` が参照文字列に置き換わる。`op read --no-newline {}` のように書く。
    pub command: String,
    /// 末尾の改行を落とす。多くの CLI は改行を付けて返す。
    #[serde(default = "yes")]
    pub trim: bool,
}

fn yes() -> bool {
    true
}

/// scheme -> 取り出し方
pub type Providers = BTreeMap<String, Provider>;

/// 宣言が無いときの既定。1Password だけを知っている。
pub fn default_providers() -> Providers {
    let mut m = BTreeMap::new();
    m.insert(
        "op".to_string(),
        Provider {
            command: "op read --no-newline {}".into(),
            trim: true,
        },
    );
    m
}

/// 1 回の render で同じ参照を何度も引かないよう覚えておく。
#[derive(Default)]
pub struct SecretCache {
    seen: BTreeMap<String, String>,
    providers: Providers,
}

impl SecretCache {
    pub fn with(providers: Providers) -> Self {
        Self {
            seen: BTreeMap::new(),
            providers,
        }
    }

    fn read(&mut self, scheme: &str, reference: &str) -> Result<String> {
        let key = format!("{scheme}://{reference}");
        if let Some(v) = self.seen.get(&key) {
            return Ok(v.clone());
        }
        let Some(provider) = self.providers.get(scheme) else {
            let known: Vec<&str> = self.providers.keys().map(String::as_str).collect();
            bail!(
                "no provider declared for `{scheme}://`. Known: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            );
        };

        // 参照は引数として渡す。シェルを経由しないので、参照に空白や記号が
        // あってもそのまま届き、注入の余地も無い。
        let mut parts = shell_words(&provider.command);
        if parts.is_empty() {
            bail!("provider `{scheme}` has an empty command");
        }
        for part in parts.iter_mut() {
            *part = part.replace("{}", reference);
        }
        let bin = parts.remove(0);

        let out = std::process::Command::new(&bin)
            .args(&parts)
            .output()
            .with_context(|| format!("failed to run `{bin}` for {scheme}://; is it installed?"))?;
        if !out.status.success() {
            bail!(
                "`{} {}` failed: {}",
                bin,
                parts.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let mut value =
            String::from_utf8(out.stdout).with_context(|| format!("{key} is not valid UTF-8"))?;
        if provider.trim {
            while value.ends_with('\n') || value.ends_with('\r') {
                value.pop();
            }
        }
        self.seen.insert(key, value.clone());
        Ok(value)
    }
}

/// 引用符を尊重した最小の分割。見るのはコマンド定義側の引用だけ。
pub fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut had_quote = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), ch) if ch == q => quote = None,
            (Some(_), ch) => cur.push(ch),
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                had_quote = true;
            }
            (None, ch) if ch.is_whitespace() => {
                if !cur.is_empty() || had_quote {
                    out.push(std::mem::take(&mut cur));
                    had_quote = false;
                }
            }
            (None, ch) => cur.push(ch),
        }
    }
    if !cur.is_empty() || had_quote {
        out.push(cur);
    }
    out
}

/// 条件ブロックを先に処理して、残らない側を落とす。
///
/// ループも関数も入れない。入れた瞬間にテンプレートが「生成先の設定ファイル
/// として読めるもの」でなくなる。ブロック単位の分岐だけなら、消える行が
/// 見えるだけで元の形は保たれる。
///
///     {{ if sennit.os == "darwin" }}
///     macos-option-as-alt = true
///     {{ end }}
///
/// 比較は == と != のみ。左辺は変数、右辺は変数か引用符付きの文字列。
/// `{{ if var }}` は「空でなければ真」。
fn strip_conditionals(
    template: &str,
    vars: &BTreeMap<String, String>,
    source: &str,
) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    // 真の側を出力しているか。ネストのために積む
    let mut stack: Vec<bool> = Vec::new();

    while let Some(i) = rest.find("{{") {
        let Some(end_rel) = rest[i + 2..].find("}}") else {
            break;
        };
        let directive = rest[i + 2..i + 2 + end_rel].trim();
        let before = &rest[..i];
        let after = &rest[i + 2 + end_rel + 2..];

        let keeping = stack.iter().all(|k| *k);
        if keeping {
            out.push_str(before);
        }

        if let Some(cond) = directive.strip_prefix("if ") {
            // 落とす側にある条件は評価しない。OS で括った中にその OS でしか
            // 存在しない変数を書く、というのが分岐の主用途で、そこを評価すると
            // 反対の OS で必ず落ちる。消える文は読まない。
            let value = if keeping {
                evaluate(cond.trim(), vars, source)?
            } else {
                false
            };
            stack.push(value);
            trim_line(&out, after, &mut rest);
            continue;
        }
        if directive == "else" {
            let Some(top) = stack.pop() else {
                bail!("{source}: `else` without `if`");
            };
            stack.push(!top);
            trim_line(&out, after, &mut rest);
            continue;
        }
        if directive == "end" {
            if stack.pop().is_none() {
                bail!("{source}: `end` without `if`");
            }
            trim_line(&out, after, &mut rest);
            continue;
        }

        // 条件でないものはそのまま残す。値の置換は次の段でやる
        if keeping {
            out.push_str(&rest[i..i + 2 + end_rel + 2]);
        }
        rest = after;
    }

    if !stack.is_empty() {
        bail!("{source}: unterminated `if`");
    }
    if stack.iter().all(|k| *k) {
        out.push_str(rest);
    }
    Ok(out)
}

/// ディレクティブだけの行は行ごと消す。残すと空行が増える。
fn trim_line<'a>(out: &str, after: &'a str, rest: &mut &'a str) {
    if out.ends_with('\n') || out.is_empty() {
        *rest = after.strip_prefix('\n').unwrap_or(after);
    } else {
        *rest = after;
    }
}

fn evaluate(cond: &str, vars: &BTreeMap<String, String>, source: &str) -> Result<bool> {
    for (op, negate) in [("==", false), ("!=", true)] {
        if let Some((l, r)) = cond.split_once(op) {
            let l = resolve(l.trim(), vars, source)?;
            let r = resolve(r.trim(), vars, source)?;
            return Ok((l == r) != negate);
        }
    }
    // 単体なら「空でなければ真」
    Ok(!resolve(cond, vars, source)?.is_empty())
}

fn resolve(token: &str, vars: &BTreeMap<String, String>, source: &str) -> Result<String> {
    if (token.starts_with('"') && token.ends_with('"') && token.len() >= 2)
        || (token.starts_with('\'') && token.ends_with('\'') && token.len() >= 2)
    {
        return Ok(token[1..token.len() - 1].to_string());
    }
    match vars.get(token) {
        Some(v) => Ok(v.clone()),
        // 環境変数は「設定されていない」が正常な状態で、それを表す値が空文字。
        // 未設定を誤りにすると `{{ if env.WORK_LAPTOP }}` が、常に設定されて
        // いる変数にしか書けなくなり、条件として使い物にならない。
        None if token.starts_with("env.") => Ok(String::new()),
        None => bail!("{source}: unknown variable `{token}` in a condition"),
    }
}

/// 条件を解決して、残る文だけにする。
///
/// 秘密を読むかどうかの判定はこの後でなければならない。生の本文を見ると、
/// 他の OS 向けに括ってある op:// まで数えてしまい、そのテンプレートは
/// このマシンでは永久に保留されたまま生成されない。
pub fn resolve_conditionals(
    template: &str,
    vars: &BTreeMap<String, String>,
    source: &str,
) -> Result<String> {
    strip_conditionals(template, vars, source)
}

pub fn expand_with(
    template: &str,
    vars: &BTreeMap<String, String>,
    source: &str,
    secrets: &mut SecretCache,
) -> Result<String> {
    let template = strip_conditionals(template, vars, source)?;
    let template = template.as_str();

    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("{source}: unterminated `{{{{`");
        };
        let key = after[..end].trim();
        if let Some((scheme, reference)) = split_reference(key) {
            out.push_str(&secrets.read(scheme, reference)?);
        } else {
            match vars.get(key) {
                Some(v) => out.push_str(v),
                None => bail!("{source}: unknown template variable `{key}`"),
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 秘密を使わないテンプレートの展開。テストは 1Password を叩かない。
    fn expand_with_test(
        template: &str,
        vars: &BTreeMap<String, String>,
        source: &str,
    ) -> Result<String> {
        expand_with(template, vars, source, &mut SecretCache::default())
    }

    fn vars() -> BTreeMap<String, String> {
        let mut v = BTreeMap::new();
        v.insert("ui.bg".into(), "#1a1b26".into());
        v.insert("normal.red".into(), "#f7768e".into());
        v
    }

    /// 分岐の主用途は「その OS にしかない設定を括る」。落とす側を評価すると、
    /// 反対の OS で必ず落ちる。
    #[test]
    fn a_condition_in_a_dropped_branch_is_not_evaluated() {
        let mut v = vars();
        v.insert("sennit.os".into(), "darwin".into());
        let out = expand_with_test(
            "{{ if sennit.os == \"linux\" }}{{ if linux.only.thing }}x = 1\n{{ end }}{{ end }}ok\n",
            &v,
            "t",
        )
        .unwrap();
        assert_eq!(out, "ok\n");
    }

    #[test]
    fn a_dropped_branch_may_hold_a_variable_that_does_not_exist() {
        let mut v = vars();
        v.insert("sennit.os".into(), "darwin".into());
        let out = expand_with_test(
            "{{ if sennit.os == \"linux\" }}x = {{ linux.only.thing }}\n{{ end }}ok\n",
            &v,
            "t",
        )
        .unwrap();
        assert_eq!(out, "ok\n");
    }

    /// 未設定は環境変数の正常な状態で、それを表す値が空文字。誤りにすると
    /// 常に設定されている変数にしか条件が書けない。
    #[test]
    fn an_unset_environment_variable_is_false_in_a_condition() {
        let out = expand_with_test(
            "{{ if env.SENNIT_NO_SUCH_VAR }}yes\n{{ else }}no\n{{ end }}",
            &vars(),
            "t",
        )
        .unwrap();
        assert_eq!(out, "no\n");
    }

    #[test]
    fn an_unset_environment_variable_compares_as_empty() {
        let out = expand_with_test(
            "{{ if env.SENNIT_NO_SUCH_VAR == \"\" }}unset\n{{ end }}",
            &vars(),
            "t",
        )
        .unwrap();
        assert_eq!(out, "unset\n");
    }

    #[test]
    fn schemes_used_lists_each_provider_once() {
        assert_eq!(
            schemes_used("{{ op://a/b }}{{ op://c/d }}{{ pass://e }}{{ ui.bg }}"),
            vec!["op".to_string(), "pass".to_string()]
        );
        assert!(schemes_used("{{ ui.bg }}").is_empty());
    }

    #[test]
    fn expands_known_variables() {
        let out = expand_with_test("bg = \"{{ ui.bg }}\"", &vars(), "t").unwrap();
        assert_eq!(out, "bg = \"#1a1b26\"");
    }

    #[test]
    fn expands_multiple_occurrences() {
        let out =
            expand_with_test("{{ ui.bg }}/{{ normal.red }}/{{ ui.bg }}", &vars(), "t").unwrap();
        assert_eq!(out, "#1a1b26/#f7768e/#1a1b26");
    }

    #[test]
    fn leaves_text_without_placeholders_untouched() {
        let src = "no placeholders here";
        assert_eq!(expand_with_test(src, &vars(), "t").unwrap(), src);
    }

    #[test]
    fn tolerates_whitespace_in_placeholder() {
        assert_eq!(
            expand_with_test("{{ui.bg}}", &vars(), "t").unwrap(),
            "#1a1b26"
        );
        assert_eq!(
            expand_with_test("{{   ui.bg   }}", &vars(), "t").unwrap(),
            "#1a1b26"
        );
    }

    /// 未知の変数は黙って空文字にせず落とす。設定が壊れたまま配置されるのを防ぐ。
    #[test]
    fn unknown_variable_is_an_error() {
        let err = expand_with_test("{{ nope }}", &vars(), "t.tmpl").unwrap_err();
        assert!(err.to_string().contains("unknown template variable"));
        assert!(err.to_string().contains("t.tmpl"));
    }

    #[test]
    fn unterminated_placeholder_is_an_error() {
        assert!(expand_with_test("{{ ui.bg", &vars(), "t").is_err());
    }

    fn cond(t: &str) -> Result<String> {
        let mut v = vars();
        v.insert("sennit.os".into(), "darwin".into());
        v.insert("sennit.profile".into(), String::new());
        strip_conditionals(t, &v, "t")
    }

    #[test]
    fn keeps_the_true_branch() {
        let out = cond("{{ if sennit.os == \"darwin\" }}\nmac\n{{ end }}\n").unwrap();
        assert_eq!(out, "mac\n");
    }

    #[test]
    fn drops_the_false_branch() {
        let out = cond("{{ if sennit.os == \"linux\" }}\nlinux\n{{ end }}\n").unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn handles_else() {
        let out = cond("{{ if sennit.os == \"linux\" }}\na\n{{ else }}\nb\n{{ end }}\n").unwrap();
        assert_eq!(out, "b\n");
    }

    #[test]
    fn not_equal_works() {
        let out = cond("{{ if sennit.os != \"linux\" }}\nmac\n{{ end }}\n").unwrap();
        assert_eq!(out, "mac\n");
    }

    /// 単体の変数は「空でなければ真」。profile 未設定を素直に書けるように。
    #[test]
    fn a_bare_variable_is_true_when_not_empty() {
        assert_eq!(cond("{{ if sennit.os }}\nx\n{{ end }}\n").unwrap(), "x\n");
        assert_eq!(cond("{{ if sennit.profile }}\nx\n{{ end }}\n").unwrap(), "");
    }

    /// 設定ファイル側に [end] のようなリテラルがあっても壊さない。
    /// 見るのは {{ }} の中だけ。
    #[test]
    fn literal_text_resembling_directives_is_untouched() {
        let out = cond("[end]\nname = 1\n").unwrap();
        assert_eq!(out, "[end]\nname = 1\n");
    }

    #[test]
    fn nesting_works() {
        let out =
            cond("{{ if sennit.os == \"darwin\" }}\n{{ if sennit.os }}\ny\n{{ end }}\n{{ end }}\n")
                .unwrap();
        assert_eq!(out, "y\n");
    }

    #[test]
    fn unbalanced_blocks_are_errors() {
        assert!(cond("{{ if sennit.os }}\nx\n").is_err());
        assert!(cond("x\n{{ end }}\n").is_err());
        assert!(cond("{{ else }}\n").is_err());
    }

    /// 条件に出てくる未知の変数も黙って偽にしない。
    #[test]
    fn unknown_variable_in_a_condition_is_an_error() {
        assert!(cond("{{ if nope == \"x\" }}\ny\n{{ end }}\n").is_err());
    }

    /// op:// を含むかどうかで、初回セットアップで展開するかが変わる。
    #[test]
    fn detects_secret_references() {
        assert!(needs_secrets("token = {{ op://Vault/Item/field }}"));
        assert!(!needs_secrets("bg = {{ ui.bg }}"));
        assert!(!needs_secrets("no placeholders"));
    }

    /// 閉じていない {{ を秘密ありと誤判定しない。
    #[test]
    fn unterminated_placeholder_is_not_a_secret() {
        assert!(!needs_secrets("{{ op://Vault"));
    }

    #[test]
    fn flattens_nested_tables() {
        let toml_src = "[ui]\nbg = \"#111\"\n\n[normal]\nred = \"#f00\"\n";
        let value: toml::Value = toml::from_str(toml_src).unwrap();
        let mut out = BTreeMap::new();
        flatten(&value, String::new(), &mut out);
        assert_eq!(out.get("ui.bg").unwrap(), "#111");
        assert_eq!(out.get("normal.red").unwrap(), "#f00");
    }
}
