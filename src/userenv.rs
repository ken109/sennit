//! `[env.windows]`: user environment variables.
//!
//! On Windows, tools that read a config path from an environment variable
//! (`XDG_CONFIG_HOME`, `BAT_CONFIG_DIR`, ...) need it set for the user, not for
//! one shell. apply writes them to `HKCU\Environment` as `REG_SZ` and tells
//! running programs with `WM_SETTINGCHANGE`; verify compares. There is no
//! counterpart elsewhere: a Unix shell's environment comes from its startup
//! files, which are already linked, so `[env]` only has a `windows` table.

use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::platform::Dirs;

/// `[env]`
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvDecl {
    /// variable -> value
    #[serde(default)]
    pub windows: BTreeMap<String, EnvValue>,
}

/// `"value"`, or `{ value = "...", if-unset = true }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    Value(String),
    Table(EnvTable),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvTable {
    pub value: String,
    /// Only set it when the user has no value of their own; never replace one.
    #[serde(default, rename = "if-unset")]
    pub if_unset: bool,
}

impl crate::de::ShortOrTable for EnvValue {
    type Short = String;
    type Table = EnvTable;
    const EXPECTING: &'static str = "a value, or a table with `value` and `if-unset`";
    fn short(s: String) -> Self {
        EnvValue::Value(s)
    }
    fn table(t: EnvTable) -> Self {
        EnvValue::Table(t)
    }
}
crate::short_or_table!(EnvValue);

impl EnvValue {
    pub fn value(&self) -> &str {
        match self {
            EnvValue::Value(v) => v,
            EnvValue::Table(t) => &t.value,
        }
    }
    pub fn if_unset(&self) -> bool {
        matches!(self, EnvValue::Table(t) if t.if_unset)
    }
}

impl EnvDecl {
    /// 読み込みで落とす検査。
    pub fn validate(&self) -> Result<()> {
        for (name, v) in &self.windows {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                bail!("[env.windows] has an invalid variable name `{name}`");
            }
            // REG_SZ で丸ごと書くと、ユーザーの Path(REG_EXPAND_SZ で、インストーラが
            // 足していった分を含む)を 1 行で置き換えてしまう
            if name.eq_ignore_ascii_case("path") {
                bail!("[env.windows] cannot set `{name}`: it would replace the whole user Path");
            }
            let value = v.value();
            if value.is_empty() {
                bail!("[env.windows] `{name}` has an empty value");
            }
            if value.contains('\0') {
                bail!("[env.windows] `{name}` contains a NUL");
            }
            if let (Some(folder), _) = crate::manifest::split_to(value) {
                if !crate::platform::KNOWN_FOLDERS.contains(&folder) {
                    bail!(
                        "[env.windows] `{name}` starts with an unknown folder `{{{folder}}}`; expected one of: {}",
                        crate::platform::KNOWN_FOLDERS
                            .iter()
                            .map(|f| format!("{{{f}}}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
        Ok(())
    }
}

/// The value as it goes into the registry. A value that starts with a folder
/// (`{home}/.config`) is a path: the folder is looked up and every `/` becomes
/// `\`. Anything else is written as it is.
pub fn resolve(value: &str, dirs: &Dirs) -> Result<String> {
    let (Some(folder), rest) = crate::manifest::split_to(value) else {
        return Ok(value.to_string());
    };
    let base = dirs.known(folder)?;
    let mut s = base.to_string_lossy().to_string();
    if !rest.is_empty() {
        s.push('\\');
        s.push_str(rest);
    }
    Ok(s.replace('/', "\\"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// differs (or is unset): write it
    Set,
    /// already that value
    Same,
    /// `if-unset`, and the user has a value of their own
    Kept,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub value: String,
    pub now: Option<String>,
    pub action: Action,
}

/// What each declared variable needs. `read` gives the user's current value.
pub fn plan(
    decl: &EnvDecl,
    dirs: &Dirs,
    read: impl Fn(&str) -> Result<Option<String>>,
) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    for (name, v) in &decl.windows {
        let value = resolve(v.value(), dirs)?;
        let now = read(name)?;
        let action = match &now {
            Some(_) if v.if_unset() => Action::Kept,
            Some(n) if *n == value => Action::Same,
            _ => Action::Set,
        };
        out.push(Change {
            name: name.clone(),
            value,
            now,
            action,
        });
    }
    Ok(out)
}

fn shown(now: &Option<String>) -> String {
    match now {
        Some(n) => format!("now {n}"),
        None => "now unset".to_string(),
    }
}

/// The user's current value, from `HKCU\Environment`.
fn read_now(name: &str) -> Result<Option<String>> {
    #[cfg(windows)]
    {
        registry::get(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Ok(None)
    }
}

/// apply の一部。書いた(--dry-run・--home では書くはずの)件数を返す。
///
/// --home を渡した試しの実行では書かない。環境変数はプロファイルに 1 つしか
/// なく、試しの home を指す値で本物を上書きしてしまう。
pub fn apply(decl: &EnvDecl, dirs: &Dirs, dry_run: bool) -> Result<usize> {
    if decl.windows.is_empty() || !crate::manifest::on_this_os(&["windows".to_string()]) {
        return Ok(0);
    }
    let changes = plan(decl, dirs, read_now)?;
    let write = !dry_run && dirs.is_real();
    let mut count = 0usize;
    for c in changes.iter().filter(|c| c.action == Action::Set) {
        count += 1;
        if !write {
            let why = if dry_run {
                ""
            } else {
                ", not written with --home"
            };
            println!(
                "  would set  {}={}  ({}{why})",
                c.name,
                c.value,
                shown(&c.now)
            );
            continue;
        }
        #[cfg(windows)]
        registry::set(&c.name, &c.value)?;
        // このあと走るフックにも見えるように
        std::env::set_var(&c.name, &c.value);
        println!("  {:>8}  {}={}", "env", c.name, c.value);
    }
    if write && count > 0 {
        // 開いているエクスプローラーなどに読み直させる。新しく開くターミナルが
        // 新しい値を受け取るのは、エクスプローラーが読み直したあと
        #[cfg(windows)]
        registry::broadcast();
    }
    Ok(count)
}

/// verify の一部。食い違っている件数を返す。
pub fn verify(decl: &EnvDecl, dirs: &Dirs) -> Result<usize> {
    if decl.windows.is_empty() || !crate::manifest::on_this_os(&["windows".to_string()]) {
        return Ok(0);
    }
    if !dirs.is_real() {
        println!(
            "  note: [env.windows] not checked with --home ({} variable(s))",
            decl.windows.len()
        );
        return Ok(0);
    }
    let changes = plan(decl, dirs, read_now)?;
    let mut bad = 0usize;
    for c in changes.iter().filter(|c| c.action == Action::Set) {
        bad += 1;
        println!(
            "  \x1b[31menv\x1b[0m      {}  (declared {}, {})",
            c.name,
            c.value,
            shown(&c.now)
        );
    }
    Ok(bad)
}

/// `HKCU\Environment` through advapi32, and the broadcast through user32.
#[cfg(windows)]
pub mod registry {
    use anyhow::{bail, Result};
    use std::ffi::{c_void, OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    type Hkey = isize;
    // ((HKEY)(ULONG_PTR)((LONG)0x80000001)): sign-extended on 64-bit
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize;
    const REG_SZ: u32 = 1;
    const RRF_RT_REG_SZ: u32 = 0x2;
    const RRF_RT_REG_EXPAND_SZ: u32 = 0x4;
    const RRF_NOEXPAND: u32 = 0x1000_0000;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_MORE_DATA: i32 = 234;
    const HWND_BROADCAST: isize = 0xffff;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: Hkey,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            len: *mut u32,
        ) -> i32;
        fn RegSetKeyValueW(
            hkey: Hkey,
            sub_key: *const u16,
            value: *const u16,
            kind: u32,
            data: *const c_void,
            len: u32,
        ) -> i32;
    }
    #[cfg(test)]
    #[link(name = "advapi32")]
    extern "system" {
        fn RegDeleteKeyValueW(hkey: Hkey, sub_key: *const u16, value: *const u16) -> i32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn SendMessageTimeoutW(
            hwnd: isize,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    /// The raw value (`REG_EXPAND_SZ` is not expanded), or None when unset.
    pub fn get(name: &str) -> Result<Option<String>> {
        let key = wide("Environment");
        let value = wide(name);
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        let mut len: u32 = 0;
        // SAFETY: the strings are NUL-terminated; a null buffer asks for the size
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != 0 && rc != ERROR_MORE_DATA {
            bail!("failed to read HKCU\\Environment\\{name} (error {rc})");
        }
        loop {
            let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
            let mut got = (buf.len() * 2) as u32;
            // SAFETY: `buf` holds `got` bytes
            let rc = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    flags,
                    std::ptr::null_mut(),
                    buf.as_mut_ptr().cast(),
                    &mut got,
                )
            };
            match rc {
                0 => {
                    let n = (got as usize / 2).min(buf.len());
                    let end = buf[..n].iter().position(|&c| c == 0).unwrap_or(n);
                    return Ok(Some(
                        OsString::from_wide(&buf[..end])
                            .to_string_lossy()
                            .to_string(),
                    ));
                }
                ERROR_FILE_NOT_FOUND => return Ok(None),
                // grew in between
                ERROR_MORE_DATA => len = got,
                _ => bail!("failed to read HKCU\\Environment\\{name} (error {rc})"),
            }
        }
    }

    pub fn set(name: &str, value: &str) -> Result<()> {
        let key = wide("Environment");
        let n = wide(name);
        let data = wide(value);
        // SAFETY: `data` is NUL-terminated and its length in bytes includes the NUL
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                n.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if rc != 0 {
            bail!("failed to write HKCU\\Environment\\{name} (error {rc})");
        }
        Ok(())
    }

    /// For tests. Removing an unset value is not an error.
    #[cfg(test)]
    pub fn delete(name: &str) -> Result<()> {
        let key = wide("Environment");
        let n = wide(name);
        // SAFETY: NUL-terminated strings
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), n.as_ptr()) };
        if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
            bail!("failed to delete HKCU\\Environment\\{name} (error {rc})");
        }
        Ok(())
    }

    /// `WM_SETTINGCHANGE` with "Environment", as the System dialog sends it.
    /// A window that does not answer within 5 s is skipped.
    pub fn broadcast() {
        let what = wide("Environment");
        let mut result = 0usize;
        // SAFETY: `what` outlives the call; the call does not keep the pointer
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                what.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn decl(toml: &str) -> Result<EnvDecl> {
        #[derive(Deserialize)]
        struct W {
            env: EnvDecl,
        }
        let w: W = toml::from_str(toml)?;
        w.env.validate()?;
        Ok(w.env)
    }

    fn dirs() -> Dirs {
        Dirs::new(PathBuf::from(r"C:\Users\me"), false)
    }

    /// dotfiles の install-packages.ps1 が設定している 4 つが書ける。
    #[test]
    fn the_variables_the_install_script_set_can_be_declared() {
        let d = decl(
            r#"
[env.windows]
XDG_CONFIG_HOME = "{home}/.config"
BAT_CONFIG_DIR = "{home}/.config/bat"
HERDR_CONFIG_PATH = "{appdata}/herdr/config.toml"
GH_CONFIG_DIR = { value = "{appdata}/GitHub CLI", if-unset = true }
"#,
        )
        .unwrap();
        let got = plan(&d, &dirs(), |_| Ok(None)).unwrap();
        let v: BTreeMap<_, _> = got
            .iter()
            .map(|c| (c.name.as_str(), c.value.as_str()))
            .collect();
        // Dirs::known joins with the host's separator; the value only ever has `\`
        let appdata = r"C:\Users\me\AppData\Roaming";
        assert_eq!(v["XDG_CONFIG_HOME"], r"C:\Users\me\.config");
        assert_eq!(v["BAT_CONFIG_DIR"], r"C:\Users\me\.config\bat");
        assert_eq!(
            v["HERDR_CONFIG_PATH"],
            format!(r"{appdata}\herdr\config.toml")
        );
        assert_eq!(v["GH_CONFIG_DIR"], format!(r"{appdata}\GitHub CLI"));
        assert!(got.iter().all(|c| c.action == Action::Set));
    }

    #[test]
    fn a_value_without_a_folder_is_written_as_it_is() {
        assert_eq!(resolve("a/b c", &dirs()).unwrap(), "a/b c");
        assert_eq!(resolve("{home}", &dirs()).unwrap(), r"C:\Users\me");
    }

    #[test]
    fn what_is_already_there_decides_the_action() {
        let d = decl(
            r#"
[env.windows]
SAME = "x"
DIFF = "x"
UNSET = "x"
MINE = { value = "x", if-unset = true }
NONE_YET = { value = "x", if-unset = true }
"#,
        )
        .unwrap();
        let now = |n: &str| -> Result<Option<String>> {
            Ok(match n {
                "SAME" => Some("x".into()),
                "DIFF" | "MINE" => Some("y".into()),
                _ => None,
            })
        };
        let got: BTreeMap<_, _> = plan(&d, &dirs(), now)
            .unwrap()
            .into_iter()
            .map(|c| (c.name, c.action))
            .collect();
        assert_eq!(got["SAME"], Action::Same);
        assert_eq!(got["DIFF"], Action::Set);
        assert_eq!(got["UNSET"], Action::Set);
        assert_eq!(got["MINE"], Action::Kept);
        assert_eq!(got["NONE_YET"], Action::Set);
    }

    #[test]
    fn a_bad_declaration_is_rejected() {
        for (toml, want) in [
            ("[env.darwin]\nA = \"x\"\n", "unknown field `darwin`"),
            (
                "[env.windows]\nA = { value = \"x\", ifunset = true }\n",
                "unknown field `ifunset`",
            ),
            ("[env.windows]\nA = \"\"\n", "empty value"),
            ("[env.windows]\nPath = \"x\"\n", "whole user Path"),
            ("[env.windows]\n\"A=B\" = \"x\"\n", "invalid variable name"),
            ("[env.windows]\nA = \"{desktop}/x\"\n", "unknown folder"),
            ("[env.windows]\nA = 1\n", "a value, or a table"),
        ] {
            let e = decl(toml).unwrap_err();
            assert!(format!("{e:#}").contains(want), "{toml}: {e:#}");
        }
    }

    /// 本物の HKCU に書いて読み戻す。CI か SENNIT_TEST_REGISTRY のときだけ。
    /// 名前は SENNIT_TEST_* だけを使い、終わったら消す。
    #[cfg(windows)]
    #[test]
    fn the_registry_round_trips() {
        if std::env::var_os("CI").is_none() && std::env::var_os("SENNIT_TEST_REGISTRY").is_none() {
            return;
        }
        let name = "SENNIT_TEST_ROUND_TRIP";
        registry::delete(name).unwrap();
        assert_eq!(registry::get(name).unwrap(), None);
        let value = r"C:\Users\me\GitHub CLI\日本語";
        registry::set(name, value).unwrap();
        let back = registry::get(name);
        registry::delete(name).unwrap();
        assert_eq!(back.unwrap().as_deref(), Some(value));
        assert_eq!(registry::get(name).unwrap(), None);

        // apply が書き、verify が通り、if-unset は上書きしない
        let real = Dirs::new(PathBuf::from(r"C:\Users\me"), true);
        let d = decl(
            "[env.windows]\nSENNIT_TEST_APPLY = \"{home}/x\"\nSENNIT_TEST_KEEP = { value = \"new\", if-unset = true }\n",
        )
        .unwrap();
        registry::set("SENNIT_TEST_KEEP", "mine").unwrap();
        let wrote = apply(&d, &real, false);
        let checked = verify(&d, &real);
        let applied = registry::get("SENNIT_TEST_APPLY");
        let kept = registry::get("SENNIT_TEST_KEEP");
        registry::delete("SENNIT_TEST_APPLY").unwrap();
        registry::delete("SENNIT_TEST_KEEP").unwrap();
        assert_eq!(wrote.unwrap(), 1);
        assert_eq!(checked.unwrap(), 0);
        assert_eq!(
            std::env::var("SENNIT_TEST_APPLY").unwrap(),
            r"C:\Users\me\x"
        );
        assert_eq!(applied.unwrap().as_deref(), Some(r"C:\Users\me\x"));
        assert_eq!(kept.unwrap().as_deref(), Some("mine"));
    }

    /// --home では書かない。
    #[cfg(windows)]
    #[test]
    fn nothing_is_written_with_a_given_home() {
        let d = decl("[env.windows]\nSENNIT_TEST_NEVER = \"{home}/x\"\n").unwrap();
        assert_eq!(apply(&d, &dirs(), false).unwrap(), 1);
        assert_eq!(registry::get("SENNIT_TEST_NEVER").unwrap(), None);
    }

    /// Windows 以外では何もしない。
    #[cfg(not(windows))]
    #[test]
    fn nothing_happens_elsewhere() {
        let d = decl("[env.windows]\nA = \"x\"\n").unwrap();
        let real = Dirs::new(PathBuf::from("/h"), true);
        assert_eq!(apply(&d, &real, false).unwrap(), 0);
        assert_eq!(verify(&d, &real).unwrap(), 0);
        assert!(std::env::var_os("A").is_none());
    }
}
