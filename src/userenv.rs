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
            crate::manifest::checked_folder(&format!("[env.windows] `{name}`"), value)?;
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

/// What `HKCU\Environment` holds for a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    /// as stored: `%USERPROFILE%` is not expanded
    pub value: String,
    /// `REG_EXPAND_SZ` rather than `REG_SZ`
    pub expand: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub value: String,
    pub now: Option<String>,
    /// write it as `REG_EXPAND_SZ`
    pub expand: bool,
    pub action: Action,
}

/// Whether two values mean the same: both expanded (`%USERPROFILE%`), compared
/// without case, and a trailing `\` does not count. Windows reads them that way.
pub fn same_value(a: &str, b: &str, expand: impl Fn(&str) -> String) -> bool {
    let norm = |s: &str| {
        let e = expand(s);
        let t = e.trim_end_matches('\\');
        // `C:\` を `C:` にしない
        let t = if t.ends_with(':') { e.as_str() } else { t };
        t.to_lowercase()
    };
    norm(a) == norm(b)
}

/// What each declared variable needs. `read` gives what the user has now, and
/// `expand` expands `%NAME%` as Windows does.
pub fn plan(
    decl: &EnvDecl,
    dirs: &Dirs,
    read: impl Fn(&str) -> Result<Option<Current>>,
    expand: impl Fn(&str) -> String,
) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    for (name, v) in &decl.windows {
        let value = resolve(v.value(), dirs)?;
        let now = read(name)?;
        let action = match &now {
            Some(_) if v.if_unset() => Action::Kept,
            Some(n) if same_value(&n.value, &value, &expand) => Action::Same,
            _ => Action::Set,
        };
        // `%` があれば展開される型で書く。既存が REG_EXPAND_SZ なら、それも残す
        // (ほかの道具が `%` 入りの値に書き戻したときに展開されなくならないように)
        let expand_sz = value.contains('%') || now.as_ref().is_some_and(|n| n.expand);
        out.push(Change {
            name: name.clone(),
            value,
            now: now.map(|n| n.value),
            expand: expand_sz,
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
fn read_now(name: &str) -> Result<Option<Current>> {
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

fn write_now(c: &Change) -> Result<()> {
    #[cfg(windows)]
    {
        registry::set(&c.name, &c.value, c.expand)
    }
    #[cfg(not(windows))]
    {
        let _ = c;
        Ok(())
    }
}

fn expand_now(s: &str) -> String {
    #[cfg(windows)]
    {
        registry::expand(s)
    }
    #[cfg(not(windows))]
    {
        s.to_string()
    }
}

/// apply の結果。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// 書いた件数
    pub written: usize,
    /// 書かずに、書くはずと出した件数(--dry-run・--home)
    pub previewed: usize,
}

/// apply の一部。
///
/// --home を渡した試しの実行では書かない。環境変数はプロファイルに 1 つしか
/// なく、試しの home を指す値で本物を上書きしてしまう。
pub fn apply(decl: &EnvDecl, dirs: &Dirs, dry_run: bool) -> Result<Applied> {
    let mut done = Applied::default();
    if decl.windows.is_empty() || !crate::manifest::on_this_os(&["windows".to_string()]) {
        return Ok(done);
    }
    let changes = plan(decl, dirs, read_now, expand_now)?;
    let write = !dry_run && dirs.is_real();
    let mut failed: Option<anyhow::Error> = None;
    for c in changes.iter().filter(|c| c.action == Action::Set) {
        if !write {
            done.previewed += 1;
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
        if let Err(e) = write_now(c) {
            failed = Some(e);
            break;
        }
        done.written += 1;
        // このあと走るフックにも見えるように
        std::env::set_var(&c.name, expand_now(&c.value));
        println!("  {:>8}  {}={}", "env", c.name, c.value);
    }
    if done.written > 0 {
        // 開いているエクスプローラーなどに読み直させる。新しく開くターミナルが
        // 新しい値を受け取るのは、エクスプローラーが読み直したあと。途中で
        // 書けなくなっても、それまでに書いた分は知らせる
        #[cfg(windows)]
        registry::broadcast();
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(done),
    }
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
    let changes = plan(decl, dirs, read_now, expand_now)?;
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
    const REG_EXPAND_SZ: u32 = 2;
    const RRF_RT_ANY: u32 = 0x0000_ffff;
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
    #[link(name = "kernel32")]
    extern "system" {
        fn ExpandEnvironmentStringsW(src: *const u16, dst: *mut u16, size: u32) -> u32;
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

    fn type_name(kind: u32) -> String {
        let name = match kind {
            0 => "REG_NONE",
            3 => "REG_BINARY",
            4 => "REG_DWORD",
            5 => "REG_DWORD_BIG_ENDIAN",
            6 => "REG_LINK",
            7 => "REG_MULTI_SZ",
            11 => "REG_QWORD",
            _ => "",
        };
        if name.is_empty() {
            format!("type {kind}")
        } else {
            name.to_string()
        }
    }

    /// The raw value (`REG_EXPAND_SZ` is not expanded) and its type, or None when
    /// unset. A value of another type (`REG_DWORD`) is an error that names it.
    pub fn get(name: &str) -> Result<Option<super::Current>> {
        let key = wide("Environment");
        let value = wide(name);
        let flags = RRF_RT_ANY | RRF_NOEXPAND;
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
            let mut kind: u32 = 0;
            // SAFETY: `buf` holds `got` bytes
            let rc = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    flags,
                    &mut kind,
                    buf.as_mut_ptr().cast(),
                    &mut got,
                )
            };
            match rc {
                0 => {
                    if kind != REG_SZ && kind != REG_EXPAND_SZ {
                        bail!(
                            "HKCU\\Environment\\{name} is a {} value, not a string; \
                             sennit only writes strings, so remove or rename it first",
                            type_name(kind)
                        );
                    }
                    let n = (got as usize / 2).min(buf.len());
                    let end = buf[..n].iter().position(|&c| c == 0).unwrap_or(n);
                    return Ok(Some(super::Current {
                        value: OsString::from_wide(&buf[..end])
                            .to_string_lossy()
                            .to_string(),
                        expand: kind == REG_EXPAND_SZ,
                    }));
                }
                ERROR_FILE_NOT_FOUND => return Ok(None),
                // grew in between
                ERROR_MORE_DATA => len = got,
                _ => bail!("failed to read HKCU\\Environment\\{name} (error {rc})"),
            }
        }
    }

    /// `REG_EXPAND_SZ` when `expand`, else `REG_SZ`.
    pub fn set(name: &str, value: &str, expand: bool) -> Result<()> {
        let key = wide("Environment");
        let n = wide(name);
        let data = wide(value);
        let kind = if expand { REG_EXPAND_SZ } else { REG_SZ };
        // SAFETY: `data` is NUL-terminated and its length in bytes includes the NUL
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                n.as_ptr(),
                kind,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if rc != 0 {
            bail!("failed to write HKCU\\Environment\\{name} (error {rc})");
        }
        Ok(())
    }

    /// `%NAME%` expanded against this process's environment. Left as it is if
    /// that fails.
    pub fn expand(s: &str) -> String {
        if !s.contains('%') {
            return s.to_string();
        }
        let src = wide(s);
        let mut cap: u32 = (src.len() as u32).max(64) * 2;
        for _ in 0..4 {
            let mut buf = vec![0u16; cap as usize];
            // SAFETY: `src` is NUL-terminated; `buf` holds `cap` characters
            let need = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), cap) };
            if need == 0 {
                break;
            }
            if need <= cap {
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                return OsString::from_wide(&buf[..end])
                    .to_string_lossy()
                    .to_string();
            }
            cap = need;
        }
        s.to_string()
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
        let got = plan(&d, &dirs(), |_| Ok(None), str::to_string).unwrap();
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
        let now = |n: &str| -> Result<Option<Current>> {
            Ok(match n {
                "SAME" => Some("x".into()),
                "DIFF" | "MINE" => Some("y".into()),
                _ => None,
            }
            .map(|value| Current {
                value,
                expand: false,
            }))
        };
        let got: BTreeMap<_, _> = plan(&d, &dirs(), now, str::to_string)
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

    /// 展開してから、大小文字と末尾の `\` を無視して比べる。
    #[test]
    fn values_are_compared_as_windows_reads_them() {
        let expand = |s: &str| s.replace("%USERPROFILE%", r"C:\Users\me");
        assert!(same_value(
            r"%USERPROFILE%\.config",
            r"C:\Users\me\.config",
            expand
        ));
        assert!(same_value(
            r"c:\users\ME\.config\",
            r"C:\Users\me\.config",
            expand
        ));
        assert!(same_value(r"C:\", r"c:\", expand));
        assert!(!same_value(r"C:\", r"C:", expand));
        assert!(!same_value(
            r"C:\Users\me\.config\bat",
            r"C:\Users\me\.config",
            expand
        ));
    }

    /// `%` を含む値、または既存が REG_EXPAND_SZ なら、REG_EXPAND_SZ で書く。
    #[test]
    fn an_expandable_value_stays_expandable() {
        let d =
            decl("[env.windows]\nPCT = \"%USERPROFILE%/x\"\nWAS = \"plain\"\nNEW = \"plain\"\n")
                .unwrap();
        let now = |n: &str| -> Result<Option<Current>> {
            Ok((n == "WAS").then(|| Current {
                value: "%OLD%".into(),
                expand: true,
            }))
        };
        let got: BTreeMap<_, _> = plan(&d, &dirs(), now, str::to_string)
            .unwrap()
            .into_iter()
            .map(|c| (c.name, c.expand))
            .collect();
        assert!(got["PCT"]);
        assert!(got["WAS"]);
        assert!(!got["NEW"]);
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
        registry::set(name, value, false).unwrap();
        let back = registry::get(name);
        registry::set(name, "%USERPROFILE%\\x", true).unwrap();
        let expandable = registry::get(name);
        registry::delete(name).unwrap();
        assert_eq!(
            back.unwrap(),
            Some(Current {
                value: value.into(),
                expand: false
            })
        );
        // 展開されずに読める
        assert_eq!(
            expandable.unwrap(),
            Some(Current {
                value: "%USERPROFILE%\\x".into(),
                expand: true
            })
        );
        assert_eq!(registry::get(name).unwrap(), None);
        assert_eq!(
            registry::expand("%USERPROFILE%\\x"),
            format!("{}\\x", std::env::var("USERPROFILE").unwrap())
        );

        // apply が書き、verify が通り、if-unset は上書きしない
        let real = Dirs::new(PathBuf::from(r"C:\Users\me"), true);
        let d = decl(
            "[env.windows]\nSENNIT_TEST_APPLY = \"{home}/x\"\nSENNIT_TEST_KEEP = { value = \"new\", if-unset = true }\n",
        )
        .unwrap();
        registry::set("SENNIT_TEST_KEEP", "mine", false).unwrap();
        let wrote = apply(&d, &real, false);
        let checked = verify(&d, &real);
        let applied = registry::get("SENNIT_TEST_APPLY");
        let kept = registry::get("SENNIT_TEST_KEEP");
        registry::delete("SENNIT_TEST_APPLY").unwrap();
        registry::delete("SENNIT_TEST_KEEP").unwrap();
        assert_eq!(wrote.unwrap().written, 1);
        assert_eq!(checked.unwrap(), 0);
        assert_eq!(
            std::env::var("SENNIT_TEST_APPLY").unwrap(),
            r"C:\Users\me\x"
        );
        assert_eq!(applied.unwrap().unwrap().value, r"C:\Users\me\x");
        assert_eq!(kept.unwrap().unwrap().value, "mine");

        // 文字列でない値に当たったら、型を書いて落とす
        let out = std::process::Command::new("reg")
            .args(["add", r"HKCU\Environment", "/v", "SENNIT_TEST_DWORD"])
            .args(["/t", "REG_DWORD", "/d", "1", "/f"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let got = registry::get("SENNIT_TEST_DWORD");
        registry::delete("SENNIT_TEST_DWORD").unwrap();
        let e = got.unwrap_err();
        assert!(e.to_string().contains("REG_DWORD"), "{e}");
    }

    /// --home では書かない。
    #[cfg(windows)]
    #[test]
    fn nothing_is_written_with_a_given_home() {
        let d = decl("[env.windows]\nSENNIT_TEST_NEVER = \"{home}/x\"\n").unwrap();
        let done = apply(&d, &dirs(), false).unwrap();
        assert_eq!((done.written, done.previewed), (0, 1));
        assert_eq!(registry::get("SENNIT_TEST_NEVER").unwrap(), None);
    }

    /// Windows 以外では何もしない。
    #[cfg(not(windows))]
    #[test]
    fn nothing_happens_elsewhere() {
        let d = decl("[env.windows]\nA = \"x\"\n").unwrap();
        let real = Dirs::new(PathBuf::from("/h"), true);
        assert_eq!(apply(&d, &real, false).unwrap(), Applied::default());
        assert_eq!(verify(&d, &real).unwrap(), 0);
        assert!(std::env::var_os("A").is_none());
    }
}
