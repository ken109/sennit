use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// 依存の種類。設定ファイルから参照されうる名前空間を分ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Command,
    Font,
    Extension,
    /// 実行ファイルを持たないもの(共有ライブラリ、証明書など)
    Library,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Command => "command",
            Kind::Font => "font",
            Kind::Extension => "extension",
            Kind::Library => "library",
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
    Apt,
    Yay,
    ZedExtension,
    /// sennit は導入に関与しない
    None,
    /// 綴りが宣言に無いもの。load で落とすのでここまで来ない
    Unknown,
}

impl Manager {
    fn parse(s: Option<&str>) -> Self {
        match s {
            None | Some("brew") => Manager::Brew,
            Some("brew-cask") => Manager::BrewCask,
            Some("mise") => Manager::Mise,
            Some("apt") => Manager::Apt,
            Some("yay") => Manager::Yay,
            // Zed 拡張は settings.json の auto_install_extensions が入れる
            Some("zed-extension") => Manager::ZedExtension,
            Some("none") => Manager::None,
            // 綴りの誤りと「関与しない」の宣言は区別する。load が弾く。
            _ => Manager::Unknown,
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
    /// このプロファイルのときだけ対象にする。空なら常に対象。
    /// OS は環境で決まるが、profile は用途で決まる(work / personal など)。
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub manager: Option<String>,
    /// Debian 系での パッケージ名。指定があると Linux では apt を使う
    #[serde(default)]
    pub apt: Option<String>,
    /// Arch 系での パッケージ名。指定があると Linux では yay を使う
    #[serde(default)]
    pub yay: Option<String>,
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

impl Package {
    pub fn kind_of(&self) -> Kind {
        match self.kind.as_deref() {
            Some("font") => Kind::Font,
            Some("extension") => Kind::Extension,
            Some("library") => Kind::Library,
            _ => Kind::Command,
        }
    }

    pub fn manager_of(&self) -> Manager {
        Manager::parse(self.manager.as_deref())
    }
}

impl Packages {
    /// 現在の OS に当てはまり、自動導入の対象になるパッケージ。
    pub fn applicable(&self) -> Vec<(String, &Package)> {
        let mut out: Vec<(String, &Package)> = self
            .packages
            .iter()
            .filter(|(_, p)| !p.optional)
            .filter(|(_, p)| p.os.is_empty() || p.os.iter().any(|o| o == current_os()))
            .filter(|(_, p)| matches_profile(&p.profiles))
            .map(|(n, p)| (n.clone(), p))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 現在の OS で sync の対象になるパッケージを返す。
    /// optional なものは自動導入しない。
    ///
    /// Linux では apt / yay の宣言があればそちらを優先する。パッケージ名が
    /// ディストリで違う(libyaml -> libyaml-dev)場合があるため、名前も
    /// そこで差し替える。どちらの宣言も無ければ既定の manager を使う
    /// (linuxbrew は Linux でも動くため)。
    pub fn installable(&self) -> Vec<Installable> {
        let linux_pm = if cfg!(target_os = "macos") {
            None
        } else {
            detect_linux_pm()
        };

        let mut out: Vec<Installable> = self
            .packages
            .iter()
            .filter(|(_, p)| !p.optional)
            .filter(|(_, p)| p.os.is_empty() || p.os.iter().any(|o| o == current_os()))
            .filter(|(_, p)| matches_profile(&p.profiles))
            .filter_map(|(name, p)| resolve(name, p, linux_pm))
            .filter(|i| {
                !matches!(
                    i.manager,
                    Manager::None | Manager::Unknown | Manager::ZedExtension
                )
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let p: Packages =
            toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        p.validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(p)
    }

    /// 綴りの誤りを黙って通さない。
    ///
    /// `manager` を打ち間違えると sync の対象から静かに外れ、それでいて
    /// verify は「入っていない」と言い続ける。宣言したはずのものが
    /// 誰にも導入されない状態になるので、読み込みで落とす。
    fn validate(&self) -> Result<()> {
        for (name, pkg) in &self.packages {
            if let Some(m) = pkg.manager.as_deref() {
                if Manager::parse(Some(m)) == Manager::Unknown {
                    bail!(
                        "package `{name}` has an unknown manager `{m}`; \
                         expected one of: brew, brew-cask, mise, apt, yay, zed-extension, none"
                    );
                }
            }
            if let Some(k) = pkg.kind.as_deref() {
                if !matches!(k, "command" | "font" | "extension" | "library") {
                    bail!(
                        "package `{name}` has an unknown kind `{k}`; \
                         expected one of: command, font, extension, library"
                    );
                }
            }
            for os in &pkg.os {
                if !matches!(os.as_str(), "darwin" | "linux") {
                    bail!("package `{name}` has an unknown os `{os}`; expected darwin or linux");
                }
            }
        }
        Ok(())
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
            // プロファイル外のものは宣言としては存在するが導入対象ではない
            let os_mismatch = (!pkg.os.is_empty() && !pkg.os.iter().any(|o| o == current_os))
                || !matches_profile(&pkg.profiles);
            let optional = pkg.optional || os_mismatch;
            let kind = pkg.kind_of();
            // パッケージ名も提供名として数える。コマンドは大抵パッケージ名と
            // 同じ(bat, fd)で、エディタ拡張は ID がそのまま設定に現れる。
            //
            // フォントだけは違う。font-cica というパッケージ名は
            // ファミリ名ではないので、参照名として数えると
            // 「宣言したが誰も参照していない」の検出が意味を失う。
            if kind != Kind::Font {
                out.insert((kind, name.clone()), optional);
            }
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

/// 現在のプロファイル。SENNIT_PROFILE で指定する。未指定なら制約なしとみなす。
pub fn current_profiles() -> Vec<String> {
    std::env::var("SENNIT_PROFILE")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// profiles が空なら常に真。指定があれば、現在のプロファイルと 1 つでも
/// 重なれば真。SENNIT_PROFILE を設定していない環境では、プロファイル付きの
/// 宣言は対象外になる。取りこぼすより余計に入れない方を選ぶ。
pub fn matches_profile(profiles: &[String]) -> bool {
    if profiles.is_empty() {
        return true;
    }
    let current = current_profiles();
    profiles.iter().any(|p| current.contains(p))
}

pub fn current_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// この Linux で使えるパッケージマネージャを 1 つ選ぶ。
/// 両方あることは通常無いが、あれば yay を優先する(Arch 系とみなす)。
fn detect_linux_pm() -> Option<Manager> {
    for (bin, m) in [("yay", Manager::Yay), ("apt-get", Manager::Apt)] {
        if which(bin) {
            return Some(m);
        }
    }
    None
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|d| {
                let p = d.join(bin);
                p.is_file()
            })
        })
        .unwrap_or(false)
}

/// 1 パッケージについて、この環境で使うマネージャと名前を決める。
fn resolve(name: &str, pkg: &Package, linux_pm: Option<Manager>) -> Option<Installable> {
    if let Some(pm) = linux_pm {
        let distro_name = match pm {
            Manager::Apt => pkg.apt.as_deref(),
            Manager::Yay => pkg.yay.as_deref(),
            _ => None,
        };
        if let Some(n) = distro_name {
            return Some(Installable {
                name: n.to_string(),
                manager: pm,
            });
        }
        // apt / yay の宣言が無く、既定が cask なら Linux では入れられない
        if Manager::parse(pkg.manager.as_deref()) == Manager::BrewCask && !is_font(pkg) {
            return None;
        }
    }
    Some(Installable {
        name: name.to_string(),
        manager: Manager::parse(pkg.manager.as_deref()),
    })
}

/// フォントの cask は Linux でも動く(supports_linux? な cask)。
fn is_font(pkg: &Package) -> bool {
    pkg.kind.as_deref() == Some("font")
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

    /// フォントのパッケージ名はファミリ名ではないので提供名に数えない。
    /// 数えてしまうと font-cica という名前が常に「参照されている」ことになり、
    /// 未使用フォントの検出が働かなくなる。
    #[test]
    fn font_package_name_is_not_a_font_name() {
        let p = parse("[packages.font-cica]\nkind = \"font\"\nprovides = [\"Cica\"]\n");
        let provided = p.provided();
        assert!(provided.contains_key(&(Kind::Font, "Cica".into())));
        assert!(!provided.contains_key(&(Kind::Font, "font-cica".into())));
    }

    /// 一方、エディタ拡張は ID がそのまま設定に現れるので数える。
    #[test]
    fn extension_package_name_is_the_reference() {
        let p =
            parse("[packages.tokyo-night]\nkind = \"extension\"\nmanager = \"zed-extension\"\n");
        assert!(p
            .provided()
            .contains_key(&(Kind::Extension, "tokyo-night".into())));
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

    fn pkg(src: &str) -> Package {
        toml::from_str(src).unwrap()
    }

    /// Linux で apt の宣言があれば、マネージャも名前もそちらを使う。
    /// libyaml -> libyaml-dev のようにディストリで名前が違うため。
    #[test]
    fn apt_declaration_overrides_manager_and_name_on_linux() {
        let p = pkg("apt = \"libyaml-dev\"\nyay = \"libyaml\"\n");
        let r = resolve("libyaml", &p, Some(Manager::Apt)).unwrap();
        assert_eq!(r.manager, Manager::Apt);
        assert_eq!(r.name, "libyaml-dev");
    }

    #[test]
    fn yay_declaration_is_used_on_arch() {
        let p = pkg("apt = \"build-essential\"\nyay = \"base-devel\"\n");
        let r = resolve("build-essential", &p, Some(Manager::Yay)).unwrap();
        assert_eq!(r.manager, Manager::Yay);
        assert_eq!(r.name, "base-devel");
    }

    /// 宣言が無ければ既定のマネージャのまま。linuxbrew は Linux でも動く。
    #[test]
    fn falls_back_to_default_manager_when_no_distro_name() {
        let p = pkg("");
        let r = resolve("bat", &p, Some(Manager::Apt)).unwrap();
        assert_eq!(r.manager, Manager::Brew);
        assert_eq!(r.name, "bat");
    }

    /// cask は Linux では入れられないので対象から外す。
    #[test]
    fn casks_are_skipped_on_linux() {
        let p = pkg("manager = \"brew-cask\"\n");
        assert!(resolve("alacritty", &p, Some(Manager::Apt)).is_none());
    }

    /// ただしフォントの cask は Linux でも入る。
    #[test]
    fn font_casks_still_apply_on_linux() {
        let p = pkg("manager = \"brew-cask\"\nkind = \"font\"\n");
        let r = resolve("font-cica", &p, Some(Manager::Apt)).unwrap();
        assert_eq!(r.manager, Manager::BrewCask);
    }

    /// macOS では apt / yay の宣言があっても無視する。
    #[test]
    fn distro_names_are_ignored_on_macos() {
        let p = pkg("apt = \"libyaml-dev\"\n");
        let r = resolve("libyaml", &p, None).unwrap();
        assert_eq!(r.manager, Manager::Brew);
        assert_eq!(r.name, "libyaml");
    }

    #[test]
    fn manager_defaults_to_brew() {
        let p = parse("[packages]\nbat = {}\n");
        let i = p.installable();
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].manager, Manager::Brew);
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;

    fn load(toml: &str) -> Result<Packages> {
        let p = std::env::temp_dir().join(format!(
            "sennit-packages-{}.toml",
            toml.bytes().map(u64::from).sum::<u64>()
        ));
        std::fs::write(&p, toml).unwrap();
        Packages::load(&p)
    }

    /// 打ち間違えると sync の対象から静かに外れる。それでいて verify は
    /// 「入っていない」と言い続けるので、誰も導入しないまま残る。
    #[test]
    fn an_unknown_manager_is_rejected() {
        let e = load("[packages]\nripgrep = { manager = \"brew-casks\" }\n").unwrap_err();
        assert!(format!("{e:#}").contains("unknown manager"), "{e:#}");
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let e = load("[packages]\ncica = { kind = \"fonts\" }\n").unwrap_err();
        assert!(format!("{e:#}").contains("unknown kind"), "{e:#}");
    }

    #[test]
    fn an_unknown_os_is_rejected() {
        let e = load("[packages]\ntrash = { os = [\"macos\"] }\n").unwrap_err();
        assert!(format!("{e:#}").contains("unknown os"), "{e:#}");
    }

    /// 「関与しない」の宣言は綴りの誤りではない
    #[test]
    fn declaring_no_manager_is_allowed() {
        let p = load("[packages]\nsomething = { manager = \"none\" }\n").unwrap();
        assert_eq!(p.packages["something"].manager_of(), Manager::None);
    }
}
