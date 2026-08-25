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

/// どのパッケージマネージャで入れるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Brew,
    BrewCask,
    Mise,
    ZedExtension,
    /// sennit は導入に関与しない
    None,
}

impl Manager {
    fn parse(s: Option<&str>) -> Self {
        match s {
            None | Some("brew") => Manager::Brew,
            Some("brew-cask") => Manager::BrewCask,
            Some("mise") => Manager::Mise,
            // Zed 拡張は settings.json の auto_install_extensions が入れる
            Some("zed-extension") => Manager::ZedExtension,
            _ => Manager::None,
        }
    }
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
    #[serde(default)]
    pub manager: Option<String>,
}

/// sync が実際に導入するパッケージ 1 件。
#[derive(Debug, Clone)]
pub struct Installable {
    pub name: String,
    pub manager: Manager,
}

#[derive(Debug, Deserialize, Default)]
pub struct Ignore {
    #[serde(default)]
    pub commands: Vec<String>,
}

impl Packages {
    /// 現在の OS で sync の対象になるパッケージを manager 別に返す。
    /// optional なものは自動導入しない。
    pub fn installable(&self) -> Vec<Installable> {
        let current_os = if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };
        let mut out: Vec<Installable> = self
            .packages
            .iter()
            .filter(|(_, p)| !p.optional)
            .filter(|(_, p)| p.os.is_empty() || p.os.iter().any(|o| o == current_os))
            .map(|(name, p)| Installable {
                name: name.clone(),
                manager: Manager::parse(p.manager.as_deref()),
            })
            .filter(|i| !matches!(i.manager, Manager::None | Manager::ZedExtension))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Packages {
        toml::from_str(src).unwrap()
    }

    #[test]
    fn package_name_is_always_provided() {
        let p = parse("[packages]\nbat = {}\n");
        let provided = p.provided();
        assert!(provided.contains_key(&(Kind::Command, "bat".into())));
    }

    /// provides はパッケージ名を置き換えるのではなく、別名として足す。
    /// nushell が nu を提供しつつ、.config/nushell の検出にも応えられるように。
    #[test]
    fn provides_adds_aliases_without_replacing_the_name() {
        let p = parse("[packages]\nnushell = { provides = [\"nu\"] }\n");
        let provided = p.provided();
        assert!(provided.contains_key(&(Kind::Command, "nushell".into())));
        assert!(provided.contains_key(&(Kind::Command, "nu".into())));
    }

    #[test]
    fn optional_packages_are_declared_but_not_installable() {
        let p = parse("[packages]\nkubectl = { optional = true }\n");
        assert!(p.provided()[&(Kind::Command, "kubectl".into())]);
        assert!(p.installable().is_empty());
    }

    #[test]
    fn ignore_list_counts_as_provided() {
        let p = parse("[packages]\n[ignore]\ncommands = [\"brew\"]\n");
        assert!(p.provided().contains_key(&(Kind::Command, "brew".into())));
    }

    #[test]
    fn fonts_and_commands_live_in_separate_namespaces() {
        let p = parse("[packages.font-x]\nkind = \"font\"\nprovides = [\"Hack\"]\n");
        let provided = p.provided();
        assert!(provided.contains_key(&(Kind::Font, "Hack".into())));
        assert!(!provided.contains_key(&(Kind::Command, "Hack".into())));
    }

    /// zed 拡張は settings.json 側が入れるので sync の対象にしない。
    #[test]
    fn zed_extensions_are_not_installed_by_sync() {
        let p =
            parse("[packages.tokyo-night]\nmanager = \"zed-extension\"\nkind = \"extension\"\n");
        assert!(p.installable().is_empty());
    }

    #[test]
    fn manager_defaults_to_brew() {
        let p = parse("[packages]\nbat = {}\n");
        let i = p.installable();
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].manager, Manager::Brew);
    }
}
