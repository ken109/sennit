use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// theme.toml をフラットな "section.key" -> 値 の表に落とす。
pub fn load_vars(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;

    let mut vars = BTreeMap::new();
    flatten(&value, String::new(), &mut vars);
    Ok(vars)
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
        if after[..end].trim().starts_with("op://") {
            return true;
        }
        rest = &after[end..];
    }
    false
}

/// `{{ key }}` を差し替えるだけの最小のテンプレート展開。
///
/// 汎用テンプレートエンジンを入れないのは、条件分岐やループを持ち込むと
/// 生成元が「設定ファイルとして読めるもの」でなくなるため。置換だけに
/// 限れば *.tmpl は元の設定とほぼ同じ見た目のまま保てる。
/// op:// で始まる参照は 1Password から取る。1 回の render で同じ参照を
/// 何度も引かないよう覚えておく。
#[derive(Default)]
pub struct SecretCache {
    seen: BTreeMap<String, String>,
}

impl SecretCache {
    fn read(&mut self, reference: &str) -> Result<String> {
        if let Some(v) = self.seen.get(reference) {
            return Ok(v.clone());
        }
        let out = std::process::Command::new("op")
            .args(["read", "--no-newline", reference])
            .output()
            .with_context(|| {
                format!("failed to run `op`; is the 1Password CLI installed? ({reference})")
            })?;
        if !out.status.success() {
            bail!(
                "`op read {reference}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let value = String::from_utf8(out.stdout)
            .with_context(|| format!("{reference} is not valid UTF-8"))?;
        self.seen.insert(reference.to_string(), value.clone());
        Ok(value)
    }
}

pub fn expand_with(
    template: &str,
    vars: &BTreeMap<String, String>,
    source: &str,
    secrets: &mut SecretCache,
) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("{source}: unterminated `{{{{`");
        };
        let key = after[..end].trim();
        if let Some(reference) = key.strip_prefix("op://") {
            let value = secrets.read(&format!("op://{reference}"))?;
            out.push_str(&value);
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
