//! Terminal policy shared by instantOS applications. No runtime or third-party dependencies.
//!
//! Plain launches attempt execution without probing the filesystem. Requests that
//! need terminal-specific arguments resolve symlinks with `read_link`. Only an
//! execution-time `NotFound` error triggers fallback; child exit codes never do.
//! Launching never modifies configuration. See [`ensure_default`] for explicit repair.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const CANDIDATES: &[&str] = &[
    "kitty",
    "ghostty",
    "wezterm",
    "foot",
    "alacritty",
    "gnome-terminal",
    "konsole",
    "xterm",
    "st",
];
const X11_CANDIDATES: &[&str] = &[
    "kitty",
    "ghostty",
    "wezterm",
    "alacritty",
    "gnome-terminal",
    "konsole",
    "xterm",
    "st",
];
const DISPLAY_VARIABLES: &[&str] = &["WAYLAND_DISPLAY", "WAYLAND_SOCKET"];

fn candidates_with_environment(
    mut variable: impl FnMut(&str) -> Option<OsString>,
) -> &'static [&'static str] {
    if DISPLAY_VARIABLES
        .iter()
        .any(|key| variable(key).is_some_and(|value| !value.is_empty()))
    {
        CANDIDATES
    } else {
        X11_CANDIDATES
    }
}

/// Candidates for explicit settings/setup selection in the current environment.
/// Ordinary launches do not call this function or inspect the display session.
pub fn candidates() -> &'static [&'static str] {
    candidates_with_environment(|key| env::var_os(key))
}

fn command_candidates(command: &Command) -> &'static [&'static str] {
    candidates_with_environment(|key| {
        command
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value.map(OsStr::to_owned))
    })
}

/// The established instantOS alias. Kept independent of XDG_CONFIG_HOME for
/// compatibility with existing desktop integrations and non-terminal aliases.
pub fn default_alias() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/instantos/default/terminal"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Profile {
    Kitty,
    Foot,
    Alacritty,
    Wezterm,
    Ghostty,
    GnomeTerminal,
    Konsole,
    Xterm,
    St,
    Other,
}

impl Profile {
    pub fn for_program(program: &Path) -> Self {
        match program.file_name().and_then(OsStr::to_str) {
            Some("kitty") => Self::Kitty,
            Some("foot" | "footclient") => Self::Foot,
            Some("alacritty") => Self::Alacritty,
            Some("wezterm") => Self::Wezterm,
            Some("ghostty") => Self::Ghostty,
            Some("gnome-terminal") => Self::GnomeTerminal,
            Some("konsole") => Self::Konsole,
            Some("xterm") => Self::Xterm,
            Some("st") => Self::St,
            _ => Self::Other,
        }
    }

    fn resolve(program: &Path) -> Self {
        // Resolve links only, without canonicalize/stat. In particular, the
        // stable `terminal` alias must not hide the selected terminal's dialect.
        let mut path = program.to_path_buf();
        for _ in 0..16 {
            match fs::read_link(&path) {
                Ok(target) if target.is_absolute() => path = target,
                Ok(target) => path = path.parent().unwrap_or(Path::new(".")).join(target),
                Err(_) => break,
            }
        }
        Self::for_program(&path)
    }
}

/// An argv-based launch request. `$TERMINAL` is an executable name/path, not a
/// shell command. Use an executable wrapper for fixed extra arguments.
#[derive(Clone, Debug)]
pub struct Launcher {
    explicit: Option<OsString>,
    preferred: Option<OsString>,
    alias: Option<PathBuf>,
    argv: Vec<OsString>,
    app_id: Option<OsString>,
    title: Option<OsString>,
    working_directory: Option<PathBuf>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            explicit: None,
            preferred: env::var_os("TERMINAL").filter(|value| !value.is_empty()),
            alias: default_alias(),
            argv: Vec::new(),
            app_id: None,
            title: None,
            working_directory: None,
        }
    }
}

impl Launcher {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            argv: vec![program.into()],
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.argv.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.argv.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn app_id(mut self, app_id: impl Into<OsString>) -> Self {
        self.app_id = Some(app_id.into());
        self
    }

    pub fn title(mut self, title: impl Into<OsString>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn working_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }

    /// Arguments for delegating this request to the shared CLI interface.
    /// The receiving process selects its terminal at execution time. Selection
    /// environment is inherited rather than serialized as a stale snapshot.
    pub fn cli_args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        for (flag, value) in [
            ("--terminal", self.explicit.as_deref()),
            ("--app-id", self.app_id.as_deref()),
            ("--title", self.title.as_deref()),
            (
                "--working-directory",
                self.working_directory.as_deref().map(Path::as_os_str),
            ),
        ] {
            if let Some(value) = value {
                args.extend([OsString::from(flag), value.to_owned()]);
            }
        }
        args.push("--".into());
        args.extend(self.argv.iter().cloned());
        args
    }

    /// An explicit override is strict: errors are reported, without selecting a
    /// different terminal behind the user's back.
    pub fn terminal(mut self, program: impl Into<OsString>) -> Self {
        self.explicit = Some(program.into());
        self
    }

    fn programs(&self) -> impl Iterator<Item = (&OsStr, bool)> {
        let explicit = self.explicit.as_deref();
        let preferred = self.preferred.as_deref().filter(|_| explicit.is_none());
        let alias = self
            .alias
            .as_deref()
            .map(Path::as_os_str)
            .filter(|alias| explicit.is_none() && Some(*alias) != preferred);
        let automatic = CANDIDATES.iter().map(OsStr::new).filter(move |candidate| {
            explicit.is_none() && Some(*candidate) != preferred && Some(*candidate) != alias
        });
        explicit
            .into_iter()
            .chain(preferred)
            .chain(alias)
            .map(|program| (program, false))
            .chain(automatic.map(|program| (program, true)))
    }

    /// Build arguments for a known executable. This performs no PATH search.
    /// An empty request performs no profile lookup either.
    pub fn command_for(&self, program: impl AsRef<OsStr>) -> Command {
        let program = program.as_ref();
        let mut cmd = Command::new(program);
        let needs_profile = !self.argv.is_empty() || self.app_id.is_some() || self.title.is_some();
        if !needs_profile {
            if let Some(dir) = &self.working_directory {
                cmd.current_dir(dir);
            }
            return cmd;
        }
        let path = Path::new(program);
        let profile = if path.components().count() > 1 {
            Profile::resolve(path)
        } else {
            Profile::for_program(path)
        };
        if profile == Profile::Wezterm {
            cmd.arg("start").arg("--always-new-process");
        }
        if profile == Profile::GnomeTerminal {
            cmd.arg("--wait");
        }
        if profile == Profile::Konsole {
            cmd.args(["--separate", "--nofork"]);
        }
        if profile == Profile::Ghostty {
            cmd.arg("--gtk-single-instance=false");
        }
        if let Some(app_id) = &self.app_id {
            let flag = match profile {
                Profile::Foot => "--app-id",
                Profile::Xterm | Profile::St => "-class",
                _ => "--class",
            };
            // st uses -c, rather than xterm's -class.
            if profile == Profile::Ghostty {
                let mut option = OsString::from("--class=");
                option.push(app_id);
                cmd.arg(option);
            } else {
                cmd.arg(if profile == Profile::St { "-c" } else { flag })
                    .arg(app_id);
            }
        }
        if let Some(title) = &self.title
            && profile != Profile::Wezterm
        {
            let flag = match profile {
                Profile::Xterm | Profile::St => "-T",
                Profile::Konsole => "-p",
                _ => "--title",
            };
            if profile == Profile::Ghostty {
                let mut option = OsString::from("--title=");
                option.push(title);
                cmd.arg(option);
            } else if profile == Profile::Konsole {
                cmd.arg(flag);
                let mut property = OsString::from("tabtitle=");
                property.push(title);
                cmd.arg(property);
            } else {
                cmd.arg(flag).arg(title);
            }
        }
        if let Some(dir) = &self.working_directory {
            cmd.current_dir(dir);
        }
        if !self.argv.is_empty() {
            cmd.arg(match profile {
                Profile::Foot | Profile::Wezterm | Profile::GnomeTerminal => "--",
                _ => "-e",
            });
            cmd.args(&self.argv);
        }
        cmd
    }

    fn attempt<T, M>(
        &self,
        mut configure: impl FnMut(&mut Command) -> M,
        mut launch: impl FnMut(&mut Command) -> io::Result<T>,
    ) -> io::Result<(T, M)> {
        let mut tried = Vec::new();
        let mut automatic_candidates: Option<&[&str]> = None;
        for (program, automatic) in self.programs() {
            if automatic
                && automatic_candidates.is_some_and(|candidates| {
                    !candidates
                        .iter()
                        .any(|candidate| OsStr::new(candidate) == program)
                })
            {
                continue;
            }
            let mut command = self.command_for(program);
            if automatic && automatic_candidates.is_none() {
                // Snapshot only the display variables, only when fallback is reached.
                // Explicit entries let get_envs reflect env_clear/env_remove as well
                // as overrides applied by configure, without probing any sockets.
                for key in DISPLAY_VARIABLES {
                    if let Some(value) = env::var_os(key) {
                        command.env(key, value);
                    }
                }
            }
            let metadata = configure(&mut command);
            if automatic && automatic_candidates.is_none() {
                automatic_candidates = Some(command_candidates(&command));
            }
            match launch(&mut command) {
                Ok(result) => return Ok((result, metadata)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    tried.push(program.to_string_lossy().into_owned());
                }
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("cannot execute {}: {error}", program.to_string_lossy()),
                    ));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no terminal could be executed (tried {}); select an installed terminal in ins settings or set TERMINAL",
                tried.join(", ")
            ),
        ))
    }

    pub fn spawn(&self) -> io::Result<Child> {
        self.spawn_with(|_| {}).map(|(child, ())| child)
    }

    /// Configure every attempt, returning the successful child's metadata.
    /// The caller owns the child and must wait for it or arrange reaping.
    pub fn spawn_with<T>(
        &self,
        configure: impl FnMut(&mut Command) -> T,
    ) -> io::Result<(Child, T)> {
        self.attempt(configure, |command| command.spawn())
    }

    /// Start an independent process group with null stdio. A lazy, shared
    /// worker reaps children without blocking the caller or polling when idle.
    /// Returns the process ID; use `spawn` when you need ownership of the child.
    pub fn spawn_detached(&self) -> io::Result<u32> {
        let reaper = reaper::shared()?;
        let (child, ()) = self.spawn_with(|command| {
            command
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        })?;
        let pid = child.id();
        reaper.watch(child);
        Ok(pid)
    }

    pub fn status(&self) -> io::Result<ExitStatus> {
        self.spawn()?.wait()
    }

    /// Replace this process; a successfully executed terminal never returns.
    pub fn exec(&self) -> io::Error {
        self.attempt::<(), ()>(|_| {}, |command| Err(command.exec()))
            .unwrap_err()
    }
}

/// Resolve an executable for settings/setup only. Launching uses exec's PATH
/// lookup directly instead of checking availability before execution.
pub fn find_executable(program: impl AsRef<OsStr>) -> Option<PathBuf> {
    let program = Path::new(program.as_ref());
    if program.components().count() > 1 {
        return is_executable(program).then(|| absolute(program)).flatten();
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let path = directory.join(program);
        is_executable(&path).then(|| absolute(&path)).flatten()
    })
}

fn absolute(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_owned())
    } else {
        env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Explicitly set an alias using an atomic rename. Executable wrappers are only
/// replaced by this explicit setter, never by automatic setup or fallback.
pub fn set_default(alias: &Path, executable: &Path) -> io::Result<()> {
    if !executable.is_absolute() || !is_executable(executable) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal target must be an absolute executable path",
        ));
    }
    let parent = alias
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "alias has no parent"))?;
    fs::create_dir_all(parent)?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".terminal-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    symlink(executable, &temporary)?;
    let result = fs::rename(&temporary, alias);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Preserve existing files/wrappers and live symlinks; fill absent or dangling
/// aliases. This operation is intentionally never called by the launch path.
pub fn ensure_default(alias: &Path) -> io::Result<()> {
    match fs::symlink_metadata(alias) {
        Ok(metadata) if metadata.file_type().is_symlink() => match fs::metadata(alias) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        },
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Respect a usable TERMINAL during explicit session setup as well.
    let target = env::var_os("TERMINAL")
        .and_then(find_executable)
        .filter(|target| target != alias)
        .or_else(|| candidates().iter().find_map(find_executable));
    match target {
        Some(target) => set_default(alias, &target),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no installed terminal found",
        )),
    }
}

/// Minimal script interface, also usable by an application's early dispatch
/// path without initializing its normal parser, UI, or async runtime.
pub fn run_cli(args: impl IntoIterator<Item = OsString>) -> i32 {
    match parse_cli(args) {
        Ok(Some(request)) => {
            let error = request.exec();
            eprintln!("instantterminal: {error}");
            if error.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            }
        }
        Ok(None) => 0,
        Err(error) => {
            eprintln!("instantterminal: {error}");
            2
        }
    }
}

fn parse_cli(args: impl IntoIterator<Item = OsString>) -> io::Result<Option<Launcher>> {
    let mut request = Launcher::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "Usage: instantterminal [--terminal EXECUTABLE] [--app-id ID] [--title TITLE] [--working-directory DIR] [-- COMMAND ARG...]\n       instantterminal --ensure-default\nTERMINAL overrides the instantOS default alias. Explicit --terminal never falls back."
                );
                return Ok(None);
            }
            Some("--ensure-default") => {
                let alias = default_alias()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
                ensure_default(&alias)?;
                return Ok(None);
            }
            Some("--" | "-e") => {
                request.argv.extend(args);
                break;
            }
            Some("--terminal" | "--app-id" | "--title" | "--working-directory") => {
                let value = args.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} requires a value", arg.to_string_lossy()),
                    )
                })?;
                match arg.to_str() {
                    Some("--terminal") => request.explicit = Some(value),
                    Some("--app-id") => request.app_id = Some(value),
                    Some("--title") => request.title = Some(value),
                    _ => request.working_directory = Some(value.into()),
                }
            }
            _ if arg.as_encoded_bytes().starts_with(b"-") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option {}", arg.to_string_lossy()),
                ));
            }
            _ => {
                request.argv.push(arg);
                request.argv.extend(args);
                break;
            }
        }
    }
    Ok(Some(request))
}

mod reaper;

#[cfg(test)]
mod tests;
