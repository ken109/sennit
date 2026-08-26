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
| `sennit verify` | Verify that everything declared actually resolves on this machine. |
| `sennit audit` | Cross-check declarations against shell history, to find ones nothing uses. |
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

`check` also looks the other way, at fonts and editor extensions that are declared but
that nothing references. That is the drift you get when a config stops using something and
the package stays behind — easy to miss, because everything keeps working. Commands are
left out of this direction on purpose: plenty of them (`bat`, `fd`, `rg`) are used daily
from the shell without appearing in any config file.

## Three ways of being wrong

Declarations go stale in three different directions, and each needs a different kind of
evidence:

| | question | evidence |
|---|---|---|
| `check` | is everything the configs need declared? | the repository |
| `verify` | does everything declared actually exist here? | this machine |
| `audit` | does anything actually use it? | shell history |

`check` is static and machine-independent, so it can gate CI. `verify` catches a
declaration that names something wrong — Homebrew's formula is `gnupg`, not `gpg`, and the
difference is invisible until you look at the machine. It only judges what can be judged:
commands on `PATH` and installed font families. GUI applications and libraries are counted
and skipped, because their absence from `PATH` means nothing.

`audit` covers the gap the other two cannot see: tools you only ever type. `rg` and `fd`
appear in no config file, so removing their declarations breaks nothing that `check` can
notice. It cross-references history with the configs, so a tool that runs automatically —
`starship`, `delta` — is not mistaken for an unused one. It never fails the build: history
is per machine and gets trimmed, so absence is a prompt to look, not proof.

None of the three can answer "what breaks if I remove this". That needs removing it and
running the install, which is a job for CI rather than for this binary.

## Installing packages

`sync` reads the same `packages.toml` and installs what is missing:

```sh
sennit sync --dry-run
sennit sync
```

It asks each manager what is already installed and only installs the difference, so
idempotency does not depend on the manager's own behaviour.

| manager | |
|---|---|
| `brew` | default; works on Linux too via Homebrew on Linux |
| `brew-cask` | macOS only, except font casks which install on Linux as well |
| `mise` | runtimes |
| `apt` / `yay` | Linux; selected automatically from what the machine has |

Package names differ between distributions, so declare them where they do:

```toml
[packages.libyaml]
apt = "libyaml-dev"      # on Debian-like systems, use apt with this name
yay = "libyaml"          # on Arch-like systems, use yay with this name
```

On Linux, an `apt` or `yay` entry decides both the manager and the name. Without one,
the default `manager` is used. Entries marked `optional`, or restricted to another OS via
`os = ["darwin"]`, are skipped. Editor extensions are declared so that `check` knows about
them, but are installed by the editor itself.

Managers run in dependency order — `apt`/`yay` first, since distribution packages are what
Homebrew sits on, then `brew`, `brew-cask`, and `mise` last because `brew` is what installs
it.

`apt` needs root. sennit works out how to get it before running anything: as root it calls
`apt-get` directly, otherwise it uses `sudo` when that is passwordless or when there is a
terminal for `sudo` to prompt on. When neither holds — a script or a container build where
`sudo` would block on a password nobody can type — it stops with an explanation rather than
hanging. `yay` is never run through `sudo`, since it refuses to run as root.

`sync` does not replace your bootstrap script: something still has to install Homebrew,
or `sudo`, before sennit can run at all.

## Status

v0.5. Minimum supported Rust version is 1.90.

The author uses it to manage [ken109/dotfiles](https://github.com/ken109/dotfiles); if you
adopt it, start with `sennit diff` and `--dry-run` before the first `apply`, since `apply`
will replace whatever is currently sitting at a managed path.

## License

MIT
