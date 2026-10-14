//! Glob matching for `[link] ignore`.
//!
//! A hand-written matcher, to keep the dependency list as it is. It supports
//! `*` and `?` inside one path component and `**` as a whole component that
//! spans any number of components. Nothing else (no `[...]`, no `!`).
//!
//! A pattern matches a path when it matches the path itself or any directory
//! above it, so ignoring a directory ignores everything underneath. Which paths
//! a pattern is compared against depends on its shape:
//!
//! - it contains a slash (`.claude/worktrees`, `**/node_modules`, `/a/*.txt`):
//!   matched from the repository root;
//! - it has no slash but a wildcard (`*.test.ts`): matched against every
//!   component, so it applies at any depth, as in gitignore;
//! - it has no slash and no wildcard (`README.md`): matched from the root only.
//!   This is how ignore has always behaved for plain names, and sennit keeps it.

use std::path::{Component, Path};

pub struct Pattern {
    segments: Vec<String>,
}

impl Pattern {
    pub fn new(raw: &str) -> Self {
        let rooted = raw.starts_with('/');
        let trimmed = raw.trim_matches('/');
        let mut segments: Vec<String> = trimmed
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(str::to_string)
            .collect();
        let has_slash = rooted || trimmed.contains('/');
        let has_wildcard = trimmed.contains(['*', '?']);
        if !has_slash && has_wildcard {
            segments.insert(0, "**".to_string());
        }
        Pattern { segments }
    }

    pub fn matches(&self, rel: &Path) -> bool {
        if self.segments.is_empty() {
            return false;
        }
        let comps: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        walk(&self.segments, &comps)
    }
}

fn walk(segs: &[String], comps: &[String]) -> bool {
    let Some((seg, rest)) = segs.split_first() else {
        // The pattern is used up: it named this path or a directory above it.
        return true;
    };
    if seg == "**" {
        return (0..=comps.len()).any(|k| walk(rest, &comps[k..]));
    }
    match comps.split_first() {
        Some((c, tail)) if component(seg, c) => walk(rest, tail),
        _ => false,
    }
}

/// One component against one pattern segment, `*` and `?` only.
fn component(pat: &str, name: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while n < name.len() {
        if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if p < pat.len() && (pat[p] == '?' || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(pat: &str, path: &str) -> bool {
        Pattern::new(pat).matches(Path::new(path))
    }

    #[test]
    fn a_wildcard_without_a_slash_applies_at_any_depth() {
        assert!(hit("*.test.ts", "a.test.ts"));
        assert!(hit("*.test.ts", ".claude/hooks/a.test.ts"));
        assert!(!hit("*.test.ts", ".claude/hooks/a.ts"));
        assert!(!hit("*.test.ts", ".claude/hooks/a.test.tsx"));
    }

    #[test]
    fn a_multi_dot_extension_matches() {
        // `rel.extension()` only ever saw `gz`, so this used to match nothing
        assert!(hit("*.tar.gz", "pkg/x.tar.gz"));
        assert!(!hit("*.tar.gz", "pkg/x.gz"));
    }

    #[test]
    fn the_old_extension_form_still_works() {
        assert!(hit("*.tmpl", ".config/starship.toml.tmpl"));
        assert!(!hit("*.tmpl", ".config/starship.toml"));
        assert!(!hit("*.tmpl", ".config/tmpl"));
    }

    #[test]
    fn a_double_star_spans_components() {
        assert!(hit("**/node_modules", "node_modules/x.js"));
        assert!(hit("**/node_modules", ".claude/hooks/node_modules/x/y.js"));
        assert!(!hit("**/node_modules", ".claude/hooks/node_modules_x/y.js"));
        assert!(hit("**/*.test.ts", ".claude/hooks/a.test.ts"));
        assert!(hit("a/**/z", "a/z"));
        assert!(hit("a/**/z", "a/b/c/z/file"));
        assert!(!hit("a/**/z", "b/a/z"));
    }

    #[test]
    fn a_pattern_with_a_slash_is_rooted() {
        assert!(hit(".claude/worktrees", ".claude/worktrees/x/y.md"));
        assert!(hit(".claude/worktrees", ".claude/worktrees"));
        assert!(!hit(".claude/worktrees", "other/.claude/worktrees/x"));
        assert!(!hit(".claude/worktrees", ".claude/worktrees-old/x"));
        assert!(hit("/a/*.txt", "a/b.txt"));
        assert!(!hit("/a/*.txt", "z/a/b.txt"));
    }

    #[test]
    fn a_star_stays_inside_one_component() {
        assert!(hit("a/*.txt", "a/b.txt"));
        assert!(!hit("a/*.txt", "a/b/c.txt"));
        assert!(hit("a/*/c", "a/b/c"));
        assert!(!hit("a/*/c", "a/c"));
    }

    #[test]
    fn a_question_mark_is_one_character() {
        assert!(hit("file?.md", "x/file1.md"));
        assert!(!hit("file?.md", "x/file12.md"));
        assert!(!hit("file?.md", "x/file.md"));
    }

    /// Plain names keep matching from the root only, as before.
    #[test]
    fn a_plain_name_is_a_rooted_prefix() {
        assert!(hit("README.md", "README.md"));
        assert!(!hit("README.md", "conf/README.md"));
        assert!(hit("conf/nvim", "conf/nvim/init.lua"));
        assert!(!hit("conf/nvim", "conf/nvim-extra/x.lua"));
        assert!(!hit("conf/nvim", "conf/nvimrc"));
    }

    #[test]
    fn an_empty_pattern_matches_nothing() {
        assert!(!hit("", "a"));
        assert!(!hit("/", "a"));
    }
}
