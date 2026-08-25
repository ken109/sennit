use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// 依存の種類。設定ファイルから参照されうる名前空間を分ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Command,
    Font,
    Extension,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Command => "command",
            Kind::Font => "font",
            Kind::Extension => "extension",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Packages {
    #[serde(default)]
    pub packages: HashMap<String, Package>,
    #[serde(default)]
    pub ignore: Ignore,
}

#[derive(Debug, Deserialize, Default)]
pub struct Package {
    /// パッケージ名以外に、設定から参照されうる別名。
    #[serde(default)]
    pub provides: Vec<String>,
    /// 省略時は command
    #[serde(default)]
    pub kind: Option<String>,
    /// 空なら全 OS。"darwin" / "linux" を指定すると、その OS でのみ宣言済みとみなす
    #[serde(default)]
    pub os: Vec<String>,
    /// setup では入れないが、設定が存在を前提に分岐しているもの
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct Ignore {
    #[serde(default)]
    pub commands: Vec<String>,
}

impl Packages {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    /// 宣言されている名前 -> optional かどうか。
    pub fn provided(&self) -> HashMap<(Kind, String), bool> {
        let mut out = HashMap::new();
        let current_os = if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };
        for (name, pkg) in &self.packages {
            // OS を限定している宣言は、対象外の OS では optional 扱いにする。
            // 設定は全 OS に配置されるが本体は入らない、という状態を可視化するため。
            let os_mismatch = !pkg.os.is_empty() && !pkg.os.iter().any(|o| o == current_os);
            let optional = pkg.optional || os_mismatch;
            let kind = match pkg.kind.as_deref() {
                Some("font") => Kind::Font,
                Some("extension") => Kind::Extension,
                _ => Kind::Command,
            };
            // パッケージ名は常に提供する。provides はそれに加える別名
            // (neovim が nvim を、nushell が nu を提供するような場合)。
            out.insert((kind, name.clone()), optional);
            for p in &pkg.provides {
                out.insert((kind, p.clone()), optional);
            }
        }
        for c in &self.ignore.commands {
            out.insert((Kind::Command, c.clone()), false);
        }
        out
    }
}
