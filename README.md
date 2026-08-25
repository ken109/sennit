# sennit

A dotfiles manager that keeps symlink semantics, and adds templating and drift detection.

> *sennit* — braided cordage, made by plaiting many strands into a single line.

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
color palette. So sennit renders those into the repository, commits the result, and symlinks
everything the same way. The live-edit loop survives for the other 90% of your configs.

## What it does

| Command | |
|---|---|
| `sennit apply` | Place symlinks. Only touches entries that need changing. |
| `sennit sync` | Install declared packages that are missing. |
| `sennit diff` | Show what an apply would change, before it happens. |
| `sennit render` | Expand templates from a single source of truth. `--check` fails if the committed output is stale. |
| `sennit check` | Verify that every dependency your configs reference is declared. |
| `sennit list` | Show the current state of every managed path. |

### Drift detection

The problem `check` solves is specific: **you update a config, and forget to update the
package list.** The config references a tool that a fresh machine will never install, and
nothing tells you until you set up a new machine months later.

`check` reads your configs with format-aware detectors rather than grepping for strings:

- commands invoked from `git config` (`core.pager`, `interactive.diffFilter`, credential helpers)
- font families in terminal and editor settings
- editor extensions declared for auto-install
- commands used in shell startup files
- the mere existence of `.config/<tool>/`, which is sometimes the *only* evidence of a dependency

Anything you deliberately do not install is declared `optional = true`, so the file records
what is intentional rather than hiding it in an ignore list.

## Configuration

Three files at the root of your dotfiles repository:

```
sennit.toml     what to symlink, what to render
theme.toml      the single source of truth for your palette
packages.toml   every package, and what names it provides
```

```toml
# sennit.toml
[link]
common = [".config", ".zshenv"]
darwin = [".hammerspoon"]
linux  = [".local"]
ignore = ["*.tmpl"]

[render]
".config/alacritty/alacritty.toml" = ".config/alacritty/alacritty.toml.tmpl"
```

Templates do substitution and nothing else — `{{ ui.bg }}` resolves against `theme.toml`.
There are deliberately no conditionals or loops: the moment a template gains control flow,
it stops being readable as the config file it produces.

## Install

```sh
brew install ken109/tap/sennit
```

Or build from source:

```sh
cargo install --git https://github.com/ken109/sennit
```

## Status

v0.2. Used daily to manage [ken109/dotfiles](https://github.com/ken109/dotfiles).

`sync` supports Homebrew (formulae and casks) and mise. It asks each manager what is
already installed and only installs the difference, so idempotency does not depend on the
manager's own behaviour. Editor extensions are declared for `check` but installed by the
editor itself.

## License

MIT
