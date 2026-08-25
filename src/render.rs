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

/// `{{ key }}` を差し替えるだけの最小のテンプレート展開。
///
/// 汎用テンプレートエンジンを入れないのは、条件分岐やループを持ち込むと
/// 生成元が「設定ファイルとして読めるもの」でなくなるため。置換だけに
/// 限れば *.tmpl は元の設定とほぼ同じ見た目のまま保てる。
pub fn expand(template: &str, vars: &BTreeMap<String, String>, source: &str) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("{source}: unterminated `{{{{`");
        };
        let key = after[..end].trim();
        match vars.get(key) {
            Some(v) => out.push_str(v),
            None => bail!("{source}: unknown template variable `{key}`"),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}
