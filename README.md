# mini-ork IDE

The native desktop interface for [mini-ork](https://github.com/SourceShift/mini-ork):
a [GPUI](https://gpui.rs) app, drawn inside a fork of
[Zed](https://github.com/SourceShift/zed-mini-ork), for starting runs, watching a
task DAG execute, and inspecting what each node actually did.

This repository is the **canonical source** of that interface crate
(`mini_ork_ui`). It is not built or run from here — see [Building](#building).
Two repositories vendor it as a git submodule.

## Where the source lives

| Repository | Submodule path | Role |
|---|---|---|
| [SourceShift/mini-ork](https://github.com/SourceShift/mini-ork) | `ui/` | the Python engine that ships the crate |
| [SourceShift/zed-mini-ork](https://github.com/SourceShift/zed-mini-ork) | `crates/mini_ork_ui/` | the IDE that builds it |

An edit here is inert until a superproject is moved onto the new commit
(`git submodule update --remote <path>` in that repository) **and the IDE is
rebuilt**. Pushing to this repository alone changes nothing a user sees — that
is the single most surprising thing about working here.

## Building

`mini_ork_ui` does not compile on its own, by design. Fourteen of its
dependencies are Zed-internal *path* crates — `gpui`, `ui`, `workspace`,
`agent_ui`, `theme`, `project`, `settings`, `git_ui`, `platform_title_bar`,
`markdown`, `http_client`, `fs`, `util`, `ui_input` — and the manifest is a
member of the Zed workspace (`edition.workspace = true`, every
`dep.workspace = true`). There is no binary and no standalone `cargo build`.

Build and run it from a `zed-mini-ork` checkout, not from here:

```bash
git clone git@github.com:SourceShift/zed-mini-ork.git
cd zed-mini-ork

script/mini-ork-check   # materialise the pinned crate, then type-check it
script/mini-ork-build   # build the IDE (release-fast profile)
script/mini-ork-ide     # launch that build
```

`script/mini-ork-pull` moves an existing checkout onto the latest released base
and rebuilds. The fork's [`MINI-ORK.md`](https://github.com/SourceShift/zed-mini-ork/blob/mini-ork/MINI-ORK.md)
has the full story.

## Repository layout

| Path | Holds |
|---|---|
| `src/mini_ork_ui.rs` | crate root — `init(cx)`, the entry Zed calls to mount the UI |
| `src/sidebar.rs` | `MiniOrkSidebar`, the view this crate exports |
| `src/graph.rs` | the run DAG and the node inspector, with its tabs |
| `src/page.rs`, `src/sections.rs` | the engine-driven page renderer |
| `src/board.rs`, `src/run_panel.rs` | the run board and its detail panel |
| `src/store.rs`, `src/ide_http.rs` | client state, and the read-only `/ide/*` surface |
| `src/components.rs`, `src/palette.rs`, `src/title_bar.rs` | shared widgets, theming, chrome |

The remaining modules (`flow`, `jump`, `loading`, `node_tab`, `projects`,
`select`, `status`, `thumb`) are supporting views.

## How it reaches mini-ork

Through the `mini-ork` CLI (`board …`, `automations …`, `init`, `serve`) as a
subprocess, and — when a server is already running — a read-only `/ide/*` HTTP
surface. See [`docs/UI.md`](https://github.com/SourceShift/mini-ork/blob/main/docs/UI.md)
and [`docs/FEATURES.md`](https://github.com/SourceShift/mini-ork/blob/main/docs/FEATURES.md).

## Contributing

- Keep the crate type-checking: `script/mini-ork-check` must pass before you push.
- A change is not delivered until a superproject is bumped *and* the IDE rebuilt.
- Follow the surrounding GPUI idiom — `cx.notify()` for re-render, observation
  for cross-entity state, `Render` / `RenderOnce` for views.

## Licence

GPL-3.0-or-later, matching the Zed crates it links against. See [`LICENSE`](LICENSE).
