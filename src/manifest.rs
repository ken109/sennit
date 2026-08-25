use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// sennit.toml の表現。
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub link: Link,
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

    pub fn is_ignored(&self, rel: &Path) -> bool {
        self.link
            .ignore
            .iter()
            // starts_with は同一パスでも真になるので、これだけで完全一致も覆う
            .any(|pat| rel.starts_with(pat))
    }
}
