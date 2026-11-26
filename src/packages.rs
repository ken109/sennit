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
    /// Windows のパッケージマネージャ
    Winget,
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
            Some("winget") => Manager::Winget,
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
    /// winget のパッケージ ID。指定があると Windows では winget を使う。
    /// `"Starship.Starship"` か `{ id = "Microsoft.PowerShell", upgrade = true }`
    #[serde(default)]
    pub winget: Option<Winget>,
}

/// `winget = ...` の中身。ID だけの文字列か、upgrade を添えた表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Winget {
    Id(String),
    Table(WingetTable),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WingetTable {
    pub id: String,
    /// 入っていても sync のたびに `winget upgrade` する
    #[serde(default)]
    pub upgrade: bool,
}

impl crate::de::ShortOrTable for Winget {
    type Short = String;
    type Table = WingetTable;
    const EXPECTING: &'static str = "a winget package ID, or a table with `id` and `upgrade`";
    fn short(s: String) -> Self {
        Winget::Id(s)
    }
    fn table(t: WingetTable) -> Self {
        Winget::Table(t)
    }
}
crate::short_or_table!(Winget);

impl Winget {
    pub fn id(&self) -> &str {
        match self {
            Winget::Id(id) => id,
            Winget::Table(t) => &t.id,
        }
    }

    pub fn upgrade(&self) -> bool {
        matches!(self, Winget::Table(t) if t.upgrade)
    }
}

/// sync が実際に導入するパッケージ 1 件。
#[derive(Debug, Clone)]
pub struct Installable {
    /// マネージャに渡す名前(winget なら ID)
    pub name: String,
    pub manager: Manager,
    /// 入っていても毎回 upgrade する(winget のみ)
    pub upgrade: bool,
    /// 「入っている」とみなすコマンド名。winget の一覧に無くても、PATH に
    /// あれば別の手段(scoop、手で入れたもの)で入っているとして入れない
    pub commands: Vec<String>,
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
    ///
    /// Windows では winget の宣言が manager と名前(ID)を決める。宣言が無ければ
    /// mise だけが残り、brew / apt / yay は入れようがないので対象から外れる。
    pub fn installable(&self) -> Vec<Installable> {
        let linux_pm = current_native_pm();

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
                         expected one of: brew, brew-cask, mise, apt, yay, winget, zed-extension, none"
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
            if let Some(w) = &pkg.winget {
                if w.id().trim().is_empty() {
                    bail!("package `{name}` has an empty winget id");
                }
            }
            for os in &pkg.os {
                if !matches!(os.as_str(), "darwin" | "linux" | "windows") {
                    bail!(
                        "package `{name}` has an unknown os `{os}`; \
                         expected darwin, linux or windows"
                    );
                }
            }
        }
        Ok(())
    }

    /// 宣言されている名前 -> optional かどうか。
    pub fn provided(&self) -> HashMap<(Kind, String), bool> {
        let mut out = HashMap::new();
        let current_os = current_os();
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
    } else if cfg!(windows) {
        "windows"
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

/// この OS で sync が導入経路を持つか。verify はこれを見て、入れようが
/// ないものを「見つからない」と報告しないようにする。
pub fn will_install(pkg: &Package) -> bool {
    will_install_with(pkg, current_native_pm())
}

/// このマシンで sync がこのパッケージを winget で入れるなら、その ID。
pub fn winget_id_here(name: &str, pkg: &Package) -> Option<String> {
    resolve(name, pkg, current_native_pm())
        .filter(|i| i.manager == Manager::Winget)
        .map(|i| i.name)
}

/// このマシンの「OS のパッケージマネージャ」。Linux では apt / yay のうち
/// 在るもの、Windows では winget、macOS では無い。
///
/// Windows で winget の有無を見ないのは、verify が「winget が無いので検証
/// しない」と黙るより、宣言どおり入っていないと言う方が役に立つから。
/// sync は winget が無ければその旨を言って落ちる。
fn current_native_pm() -> Option<Manager> {
    if cfg!(target_os = "macos") {
        None
    } else if cfg!(windows) {
        Some(Manager::Winget)
    } else {
        detect_linux_pm()
    }
}

/// 判定の本体。マネージャを引数で受けるのは、関数の中で cfg! を見ると
/// 一方の OS では常に同じ答えになり、テストが何も押さえられないため。
///
/// sync が使うのと同じ resolve に訊く。ここで条件を書き写すと必ずずれる。
/// 実際、macOS を先に真で返していたせいで、`manager = "apt"` を os 無しで
/// 書いた宣言が sync からは消え、verify だけが missing と言い続けていた。
pub fn will_install_with(pkg: &Package, native_pm: Option<Manager>) -> bool {
    resolve("", pkg, native_pm).is_some()
}

/// 1 パッケージについて、この環境で使うマネージャと名前を決める。
///
/// `native_pm` は OS のパッケージマネージャ(apt / yay / winget)。macOS では None。
fn resolve(name: &str, pkg: &Package, native_pm: Option<Manager>) -> Option<Installable> {
    let commands = if pkg.provides.is_empty() {
        vec![name.to_string()]
    } else {
        pkg.provides.clone()
    };
    let plain = |name: &str, manager| Installable {
        name: name.to_string(),
        manager,
        upgrade: false,
        commands: commands.clone(),
    };
    let manager = Manager::parse(pkg.manager.as_deref());

    if native_pm == Some(Manager::Winget) {
        // winget の宣言が manager と ID を決める。apt / yay と同じ規則
        if let Some(w) = &pkg.winget {
            return Some(Installable {
                name: w.id().to_string(),
                manager: Manager::Winget,
                upgrade: w.upgrade(),
                commands,
            });
        }
        return match manager {
            // ID の宣言が無ければパッケージ名を ID として使う
            Manager::Winget => Some(plain(name, Manager::Winget)),
            // mise は Windows でも動く。関与しない宣言はそのまま
            Manager::Mise | Manager::None | Manager::ZedExtension | Manager::Unknown => {
                Some(plain(name, manager))
            }
            // brew / cask / apt / yay は Windows に無い
            _ => None,
        };
    }

    if let Some(pm) = native_pm {
        let distro_name = match pm {
            Manager::Apt => pkg.apt.as_deref(),
            Manager::Yay => pkg.yay.as_deref(),
            _ => None,
        };
        if let Some(n) = distro_name {
            return Some(plain(n, pm));
        }
        // apt / yay の宣言が無く、既定が cask なら Linux では入れられない
        if manager == Manager::BrewCask && !is_font(pkg) {
            return None;
        }
    }
    // Linux のマネージャが無い環境(macOS)で dpkg-query / pacman を呼びに
    // 行かない。os を書き忘れた `manager = "apt"` 1 件で、sync 全体が
    // 最初のマネージャで落ちていた。
    //
    // cfg! ではなく native_pm で判断する。関数の中で OS を見ると、片方の
    // OS では常に同じ答えになってテストが何も押さえられない。
    if !matches!(native_pm, Some(Manager::Apt | Manager::Yay))
        && matches!(manager, Manager::Apt | Manager::Yay)
    {
        return None;
    }
    // winget は Windows にしか無い
    if manager == Manager::Winget {
        return None;
    }
    Some(plain(name, manager))
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

    /// Windows では winget の宣言がマネージャと ID を決める。
    #[test]
    fn a_winget_id_decides_the_manager_on_windows() {
        let p = pkg("winget = \"Starship.Starship\"\n");
        let r = resolve("starship", &p, Some(Manager::Winget)).unwrap();
        assert_eq!(r.manager, Manager::Winget);
        assert_eq!(r.name, "Starship.Starship");
        assert!(!r.upgrade);
        // PATH で探すのはパッケージ名(provides が無いとき)
        assert_eq!(r.commands, vec!["starship".to_string()]);
        // ほかの OS では読まない
        assert_eq!(
            resolve("starship", &p, None).unwrap().manager,
            Manager::Brew
        );
        assert_eq!(
            resolve("starship", &p, Some(Manager::Apt)).unwrap().manager,
            Manager::Brew
        );
    }

    /// 毎回 upgrade する宣言は表で書く。
    #[test]
    fn a_winget_table_can_ask_for_an_upgrade_every_time() {
        let p = pkg("provides = [\"pwsh\"]\nmanager = \"none\"\nwinget = { id = \"Microsoft.PowerShell\", upgrade = true }\n");
        let r = resolve("powershell", &p, Some(Manager::Winget)).unwrap();
        assert_eq!(r.manager, Manager::Winget);
        assert_eq!(r.name, "Microsoft.PowerShell");
        assert!(r.upgrade);
        assert_eq!(r.commands, vec!["pwsh".to_string()]);
    }

    /// brew / cask / apt / yay は Windows に無い。mise と none は意味を保つ。
    #[test]
    fn without_a_winget_id_only_mise_and_none_remain_on_windows() {
        for m in ["brew", "brew-cask", "apt", "yay"] {
            let p = pkg(&format!("manager = \"{m}\"\n"));
            assert!(resolve("x", &p, Some(Manager::Winget)).is_none(), "{m}");
        }
        assert!(resolve("x", &pkg(""), Some(Manager::Winget)).is_none());
        let mise = resolve("go", &pkg("manager = \"mise\"\n"), Some(Manager::Winget)).unwrap();
        assert_eq!(mise.manager, Manager::Mise);
        // `manager = "winget"` はパッケージ名を ID として使う
        let w = resolve(
            "Git.Git",
            &pkg("manager = \"winget\"\n"),
            Some(Manager::Winget),
        )
        .unwrap();
        assert_eq!((w.manager, w.name.as_str()), (Manager::Winget, "Git.Git"));
        // ほかの OS では winget を呼ばない
        assert!(resolve("Git.Git", &pkg("manager = \"winget\"\n"), None).is_none());
        assert!(resolve(
            "Git.Git",
            &pkg("manager = \"winget\"\n"),
            Some(Manager::Apt)
        )
        .is_none());
    }

    /// dotfiles の packages.toml の書き方がそのまま読める。
    #[test]
    fn the_inline_winget_key_parses_alongside_the_others() {
        let p = parse(
            "[packages]\nripgrep = { provides = [\"rg\"], winget = \"BurntSushi.ripgrep.MSVC\" }\n\n[packages.git]\napt = \"git\"\nwinget = \"Git.Git\"\n",
        );
        assert_eq!(
            p.packages["ripgrep"].winget.as_ref().map(Winget::id),
            Some("BurntSushi.ripgrep.MSVC")
        );
        assert_eq!(
            p.packages["git"].winget.as_ref().map(Winget::id),
            Some("Git.Git")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn manager_defaults_to_brew() {
        let p = parse("[packages]\nbat = {}\n");
        let i = p.installable();
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].manager, Manager::Brew);
    }

    /// Windows に brew は無い。winget の宣言があるものだけが入る。
    #[cfg(windows)]
    #[test]
    fn only_winget_packages_are_installed_on_windows() {
        let p = parse("[packages]\nbat = {}\nfd = { winget = \"sharkdp.fd\" }\n");
        let i = p.installable();
        assert_eq!(i.len(), 1);
        assert_eq!((i[0].manager, i[0].name.as_str()), (Manager::Winget, "sharkdp.fd"));
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;

    fn pkg(toml: &str) -> Package {
        toml::from_str(toml).unwrap()
    }

    /// macOS で dpkg-query / pacman を呼びに行かない。
    ///
    /// os を書き忘れた `manager = "apt"` が 1 件あるだけで、apt は
    /// ORDER の先頭なので sync 全体が最初のマネージャで落ちていた。
    /// --dry-run でも落ちるので、宣言を直すまで何も見られない。
    #[test]
    fn a_distro_manager_is_not_used_where_there_is_none() {
        let p = pkg("manager = \"apt\"\n");
        assert!(resolve("libyaml", &p, None).is_none());
        assert!(resolve("libyaml", &p, Some(Manager::Apt)).is_some());
        // 既定の brew は当然通る
        let q = pkg("");
        assert!(resolve("ripgrep", &q, None).is_some());
    }

    /// sync と verify が同じ答えを出す。
    ///
    /// macOS を先に真で返していた頃は、`manager = "apt"` を os 無しで書くと
    /// sync の対象から消える一方 verify は missing と言い続け、利用者に
    /// できることが何も無い状態になっていた。
    #[test]
    fn a_distro_manager_without_os_is_not_installable_on_macos() {
        let p = pkg("manager = \"apt\"\n");
        // macOS では Linux のマネージャが無い
        assert!(!will_install_with(&p, None));
        assert!(will_install_with(&p, Some(Manager::Apt)));
    }

    /// verify が「入れようがないもの」を missing と言わない。
    ///
    /// Linux では apt / yay の名前が無い cask に導入経路が無い。provides を
    /// 書いていると verify だけが永久に落ち続ける状態になっていた。
    #[cfg(not(windows))]
    #[test]
    fn a_cask_without_a_distro_name_is_not_installable_on_linux() {
        let cask = pkg("manager = \"brew-cask\"\nprovides = [\"op\"]\n");
        let with_apt = pkg("manager = \"brew-cask\"\nprovides = [\"op\"]\napt = \"op\"\n");
        let font = pkg("manager = \"brew-cask\"\nkind = \"font\"\nprovides = [\"X\"]\n");
        if cfg!(target_os = "macos") {
            assert!(will_install(&cask));
        } else {
            assert!(!will_install(&cask));
        }
        assert!(will_install(&with_apt));
        assert!(will_install(&font));
    }

    /// Windows has no brew; a package is held to what winget (or mise) installs.
    #[cfg(windows)]
    #[test]
    fn only_what_winget_or_mise_installs_is_installable_on_windows() {
        assert!(!will_install(&pkg("manager = \"brew\"\n")));
        assert!(will_install(&pkg("winget = \"Starship.Starship\"\n")));
    }

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

    #[test]
    fn an_unknown_winget_key_is_rejected() {
        // upgrades と書いて毎回の upgrade が黙って消えるのを防ぐ
        let e = load(
            "[packages]\npwsh = { winget = { id = \"Microsoft.PowerShell\", upgrades = true } }\n",
        )
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("unknown field `upgrades`"),
            "{e:#}"
        );
        let e = load("[packages]\npwsh = { winget = \" \" }\n").unwrap_err();
        assert!(format!("{e:#}").contains("empty winget id"), "{e:#}");
    }

    #[test]
    fn winget_is_a_known_manager() {
        let p = load("[packages]\n\"Git.Git\" = { manager = \"winget\" }\n").unwrap();
        assert_eq!(p.packages["Git.Git"].manager_of(), Manager::Winget);
    }

    /// 「関与しない」の宣言は綴りの誤りではない
    #[test]
    fn declaring_no_manager_is_allowed() {
        let p = load("[packages]\nsomething = { manager = \"none\" }\n").unwrap();
        assert_eq!(p.packages["something"].manager_of(), Manager::None);
    }
}
