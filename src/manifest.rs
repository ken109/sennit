use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// sennit.toml の表現。
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub link: Link,
    /// 出力パス -> テンプレートパス
    #[serde(default)]
    pub render: std::collections::BTreeMap<String, String>,
    /// 配置後に走らせる処理
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<String, crate::hooks::Hook>,
    /// テンプレートに渡すデータファイル。既定は theme.toml。
    #[serde(default)]
    pub data: Vec<String>,
    /// パス -> 8進のモード。
    ///
    /// symlink 方式なのでリポジトリ側の権限がそのまま見える。宣言しておくと
    /// apply が揃え、verify が検査する。
    #[serde(default)]
    pub modes: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Link {
    #[serde(default)]
    pub common: Vec<String>,
    #[serde(default)]
    pub darwin: Vec<String>,
    #[serde(default)]
    pub linux: Vec<String>,
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest: {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("failed to parse manifest: {}", path.display()))
    }

    /// 現在の OS で配置対象になるトップレベルのパス。
    pub fn targets(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.link.common.iter().map(String::as_str).collect();
        let os_specific = if cfg!(target_os = "macos") {
            &self.link.darwin
        } else {
            &self.link.linux
        };
        out.extend(os_specific.iter().map(String::as_str));
        out
    }

    /// このパスに宣言されたモード。前方一致で最も長いものを採る。
    pub fn mode_for(&self, rel: &Path) -> Option<u32> {
        self.modes
            .iter()
            .filter(|(pat, _)| rel.starts_with(pat))
            .max_by_key(|(pat, _)| pat.len())
            .and_then(|(_, m)| u32::from_str_radix(m, 8).ok())
    }

    /// ignore のパターンは 2 種類だけ扱う。
    /// - `*.ext` : 拡張子一致
    /// - それ以外: パスの前方一致(同一パスも含む)
    pub fn is_ignored(&self, rel: &Path) -> bool {
        self.link.ignore.iter().any(|pat| {
            if let Some(ext) = pat.strip_prefix("*.") {
                rel.extension().is_some_and(|e| e == ext)
            } else {
                rel.starts_with(pat)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(ignore: &[&str]) -> Manifest {
        Manifest {
            link: Link {
                common: vec![],
                darwin: vec![],
                linux: vec![],
                ignore: ignore.iter().map(|s| s.to_string()).collect(),
            },
            render: Default::default(),
            hooks: Default::default(),
            data: Default::default(),
            modes: Default::default(),
        }
    }

    #[test]
    fn ignores_by_extension_glob() {
        let m = manifest(&["*.tmpl"]);
        assert!(m.is_ignored(Path::new(".config/starship.toml.tmpl")));
        assert!(!m.is_ignored(Path::new(".config/starship.toml")));
    }

    #[test]
    fn ignores_by_path_prefix() {
        let m = manifest(&[".config/secret"]);
        assert!(m.is_ignored(Path::new(".config/secret/token")));
        // 前方一致はディレクトリ自身にも効く
        assert!(m.is_ignored(Path::new(".config/secret")));
        assert!(!m.is_ignored(Path::new(".config/public/token")));
    }

    fn with_modes(pairs: &[(&str, &str)]) -> Manifest {
        let mut m = manifest(&[]);
        for (k, v) in pairs {
            m.modes.insert(k.to_string(), v.to_string());
        }
        m
    }

    #[test]
    fn mode_is_read_as_octal() {
        let m = with_modes(&[(".npmrc", "600")]);
        assert_eq!(m.mode_for(Path::new(".npmrc")), Some(0o600));
    }

    /// 前方一致で最も長い宣言を採る。ディレクトリ全体に指定しつつ、
    /// その中の 1 つだけ変えられるように。
    #[test]
    fn the_longest_matching_declaration_wins() {
        let m = with_modes(&[(".ssh", "700"), (".ssh/config", "600")]);
        assert_eq!(m.mode_for(Path::new(".ssh/config")), Some(0o600));
        assert_eq!(m.mode_for(Path::new(".ssh/known_hosts")), Some(0o700));
    }

    #[test]
    fn no_declaration_means_no_opinion() {
        let m = manifest(&[]);
        assert_eq!(m.mode_for(Path::new(".npmrc")), None);
    }

    #[test]
    fn empty_ignore_matches_nothing() {
        let m = manifest(&[]);
        assert!(!m.is_ignored(Path::new(".config/anything")));
    }
}
