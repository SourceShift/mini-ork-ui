# `mini_ork_ui` — the mini-ork IDE crate

The source of the **mini-ork IDE**, the native (GPUI) user interface for
mini-ork. It is a library crate, drawn inside a fork of Zed.

**This repository is the canonical home of the source.** Two superprojects
vendor it as a git submodule, so an edit pushed here reaches both:

| Superproject | Submodule path |
|---|---|
| [`SourceShift/mini-ork`](https://github.com/SourceShift/mini-ork) — the Python program | `ui/` |
| [`SourceShift/zed-mini-ork`](https://github.com/SourceShift/zed-mini-ork) — the IDE (Zed fork) | `crates/mini_ork_ui/` |

Each pins a commit; run `git submodule update --remote` there to move onto the
latest `main`.

## It does not build here

`mini_ork_ui` cannot compile standalone. Fourteen of its dependencies are
Zed-internal path crates (`gpui`, `ui`, `workspace`, `agent_ui`, `theme`,
`project`, `settings`, `git_ui`, `platform_title_bar`, `markdown`,
`http_client`, `fs`, `util`, `ui_input`) and its manifest is a Zed-workspace
member (`edition.workspace = true`, every `dep.workspace = true`). There is no
binary and no standalone `cargo build` — a change here is only visible through
the IDE, built in a `zed-mini-ork` checkout. See that repo's `MINI-ORK.md`.

## How it reaches mini-ork

Through the `mini-ork` CLI (`board …`, `automations …`, `init`, `serve`) as a
subprocess, and — when a server is already running — the read-only `/ide/*` HTTP
surface. See [`docs/UI.md`](https://github.com/SourceShift/mini-ork/blob/main/docs/UI.md)
and [`docs/FEATURES.md`](https://github.com/SourceShift/mini-ork/blob/main/docs/FEATURES.md).

## Licence

GPL-3.0-or-later, matching the Zed editor crates it links against. See
[`LICENSE`](LICENSE).
