# sennit

A dotfiles manager that keeps symlink semantics, and adds templating and drift detection.

> *sennit* — braided cordage, made by plaiting many strands into a single line.

[![crates.io](https://img.shields.io/crates/v/sennit.svg)](https://crates.io/crates/sennit)
[![CI](https://github.com/ken109/sennit/actions/workflows/ci.yml/badge.svg)](https://github.com/ken109/sennit/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/sennit.svg)](LICENSE)

```sh
brew install ken109/tap/sennit   # or: cargo install sennit
```

## Why another one?

Most dotfiles managers make you choose between two models, and both cost something:

- **Symlink-based** (GNU Stow, hand-rolled scripts) — `~/.config/nvim/init.lua` *is* the
  file in your repository. Edit it in place and it shows up in `git diff`. But there is no
  templating, no way to preview what an apply will change, and nothing checks that the
  packages your configs depend on are actually installed.
- **Copy-based** (chezmoi) — you get templating and per-machine variants, but the source
  and the target are different files. Editing `~/.config` directly is a mistake that gets
  silently reverted on the next apply.

sennit takes the position that this is a false choice. **Templating and symlinking are only
in tension for the files that actually need templating** — usually a handful that carry a
color palette. So sennit renders those into your repository, commits the result, and
symlinks everything the same way. The live-edit loop survives for every other config.

## Quick start

In the root of your dotfiles repository:

```toml
# sennit.toml — what to place
[link]
common = [".config", ".zshenv"]
darwin = [".hammerspoon"]
linux  = [".local"]
ignore = ["*.tmpl"]
```

```sh
sennit diff    # see what would change
sennit apply   # place the symlinks
```

`sennit` looks for `sennit.toml` by walking up from the current directory, so it works
from anywhere inside your repository.

Directories are linked file by file rather than as a whole, so that a tool writing into
`~/.config/something/` does not make untracked files appear inside your repository.

## Commands

| | |
|---|---|
| `sennit apply` | Place symlinks. Only touches entries that need changing. `--dry-run` to preview. |
| `sennit diff` | Show what an apply would change, before it happens. |
| `sennit list` | Show the current state of every managed path. |
| `sennit render` | Expand templates from a single source of truth. `--check` fails if the committed output is stale. |
| `sennit check` | Verify that every dependency your configs reference is declared. |
| `sennit sync` | Install declared packages that are missing. |

Every path is classified as one of four states, and only the ones that need work are
touched:

| | |
|---|---|
| `linked` | already pointing at the right file |
| `missing` | nothing is there |
| `wrong` | a symlink pointing somewhere else |
| `occupied` | a real file or directory is in the way |

## Templating

Add a `theme.toml` (or any TOML file of values) and declare what is generated from what:

```toml
# sennit.toml
[render]
".config/alacritty/alacritty.toml" = ".config/alacritty/alacritty.toml.tmpl"
```

```toml
# theme.toml
[ui]
bg = "#1a1b26"
fg = "#c0caf5"
```

```toml
# .config/alacritty/alacritty.toml.tmpl
[colors.primary]
background = "{{ ui.bg }}"
foreground = "{{ ui.fg }}"
```

Templates do substitution and nothing else. There are deliberately no conditionals or
loops: the moment a template gains control flow, it stops being readable as the config
file it produces. An unknown variable is an error rather than an empty string, so a typo
cannot quietly ship a broken config.

Generated files are meant to be committed. Run `sennit render --check` in CI to catch the
case where a template was edited but the output was not regenerated.

## Drift detection

The problem `check` solves is specific: **you update a config, and forget to update the
package list.** The config references a tool that a fresh machine will never install, and
nothing tells you until you set up that machine months later.

```toml
# packages.toml
[packages]
git-delta = { provides = ["delta"] }   # the command it installs is named differently
neovim    = { provides = ["nvim"] }

[packages.font-hack-nerd-font]
manager  = "brew-cask"
kind     = "font"
provides = ["Hack Nerd Font Mono"]

[packages.kubectl]
optional = true   # known dependency, deliberately not installed automatically

[ignore]
commands = ["brew", "git", "curl"]   # provided by the system or the bootstrap
```

`check` reads your configs with format-aware detectors rather than grepping for strings:

- commands invoked from `git config` (`core.pager`, `interactive.diffFilter`, credential helpers)
- font families in terminal and editor settings
- editor extensions declared for auto-install
- commands used in shell startup files
- the mere existence of `.config/<tool>/`, which is sometimes the *only* evidence of a dependency

Anything you deliberately do not install is declared `optional = true`, so the file records
what is intentional rather than hiding it in an ignore list.

## Installing packages

`sync` reads the same `packages.toml` and installs what is missing:

```sh
sennit sync --dry-run
sennit sync
```

It asks each manager what is already installed and only installs the difference, so
idempotency does not depend on the manager's own behaviour. Supported managers are
Homebrew (`brew`, `brew-cask`) and `mise`. Entries marked `optional`, or restricted to
another OS via `os = ["darwin"]`, are skipped. Editor extensions are declared so that
`check` knows about them, but are installed by the editor itself.

`sync` does not replace your bootstrap script. Installing the OS-level packages that
Homebrew itself needs is still a job for shell.

## Status

v0.2. Minimum supported Rust version is 1.90.

The author uses it to manage [ken109/dotfiles](https://github.com/ken109/dotfiles); if you
adopt it, start with `sennit diff` and `--dry-run` before the first `apply`, since `apply`
will replace whatever is currently sitting at a managed path.

## License

MIT
