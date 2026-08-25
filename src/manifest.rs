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

    #[test]
    fn empty_ignore_matches_nothing() {
        let m = manifest(&[]);
        assert!(!m.is_ignored(Path::new(".config/anything")));
    }
}
