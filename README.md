# instantOS-libs

Small shared Rust crates for instantOS projects. Consumers use Git dependencies;
Cargo.lock pins the exact revision. There is no crates.io publishing workflow.

## instantos-terminal

A synchronous, dependency-free Unix terminal launcher shared by instantWM,
instantCLI and instantMENU. It supports kitty, foot/footclient, alacritty,
wezterm, ghostty, gnome-terminal, konsole, xterm and st. Unknown executables and
custom wrappers are treated as accepting `--class`, `--title` and `-e`.
Wezterm has no startup title option, so title requests are omitted for it.

```toml
[dependencies]
instantos-terminal = { git = "https://github.com/instantOS/instantOS-libs" }
```

```rust,no_run
use instantos_terminal::Launcher;

Launcher::new("nvim")
    .arg("notes with spaces.md")
    .app_id("scratchpad_notes")
    .title("Notes")
    .launch()?;
# Ok::<(), std::io::Error>(())
```

Use `Launcher::default()` for an ordinary terminal window. `spawn_with` lets a
window manager supply activation environment, process-group and stdio policy
for every attempt while retaining ownership of the resulting child.
`status` waits for the terminal; `exec` replaces the caller process.

Selection order:

1. Explicit `.terminal(...)` override, if supplied. This is strict and never
   falls back to a different terminal.
2. `$TERMINAL`, interpreted as one executable name or path, without shell parsing.
3. `$HOME/.config/instantos/default/terminal`, the existing instantOS alias.
4. Backend-compatible fallback candidates. Automatic X11 selection excludes foot.

A successful plain launch performs no config read, availability probe or profile
lookup and starts no helper. The kernel resolves the alias during execution.
Requests containing command arguments, app ID or title need the terminal's
argument dialect: absolute/relative executable paths are resolved with
`read_link`, without a preliminary availability check or canonicalization.
Selection is evaluated for each launch; changing the alias takes effect without
restarting the session. No daemon, filesystem watcher or environment cache is
required.

Only execution errors with `ErrorKind::NotFound` trigger fallback. Permission
errors and child exit statuses are returned directly. A terminal which starts
and crashes does not trigger another launch. Installations with a known-broken
terminal should explicitly select a working one. Fallback never changes the
saved preference.

`ensure_default` fills a missing alias or repairs a dangling link during explicit
setup. It preserves custom files and live symlinks. `set_default` changes a
preference using atomic symlink replacement. Configuration writes never happen
as a side effect of opening a terminal. The alias intentionally retains its
established location independently of XDG_CONFIG_HOME.

## Script interface

```sh
cargo install --git https://github.com/instantOS/instantOS-libs instantos-terminal
instantterminal
instantterminal --app-id scratchpad_notes --title Notes -- nvim notes.md
instantterminal --terminal foot -- sh -c 'printf "hello\n"; read answer'
instantterminal --ensure-default
```

instantCLI exposes the same interface as `ins terminal`, and instantMENU as
`instantmenu --terminal`. Both dispatch before normal CLI/UI/runtime
initialization and exec the terminal. Scripts can use these installed binaries
without an additional runtime package. Script entry points incur the cost of
loading their executable; native consumers link the crate and launch directly.
Use the standalone `instantterminal` binary when the smallest script entry point
is needed. `-e` is also accepted as a command delimiter for application-launcher
integration. Prefer `--` in new scripts.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add crates only when there is a concrete shared consumer. Keep application UI,
compositor integration and async runtimes out of this library. Breaking API
changes are acceptable: update consumers together and refresh their Git lockfile
entries with `cargo update -p instantos-terminal`.
