use super::*;

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = env::temp_dir().join(format!(
            "instantos-terminal-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn executable(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request() -> Launcher {
    Launcher {
        explicit: None,
        preferred: None,
        alias: None,
        backend: Backend::Wayland,
        argv: vec!["printf".into(), "hello world".into(), "--flag".into()],
        app_id: Some("scratchpad_notes".into()),
        title: Some("Notes with spaces".into()),
        working_directory: None,
    }
}
fn args(request: &Launcher, program: &Path) -> Vec<OsString> {
    request
        .command_for(program)
        .get_args()
        .map(OsStr::to_owned)
        .collect()
}

#[test]
fn terminal_dialects_preserve_argv() {
    let request = request();
    for (program, prefix) in [
        (
            "foot",
            vec![
                "--app-id",
                "scratchpad_notes",
                "--title",
                "Notes with spaces",
                "--",
            ],
        ),
        (
            "alacritty",
            vec![
                "--class",
                "scratchpad_notes",
                "--title",
                "Notes with spaces",
                "-e",
            ],
        ),
        (
            "kitty",
            vec![
                "--class",
                "scratchpad_notes",
                "--title",
                "Notes with spaces",
                "-e",
            ],
        ),
        (
            "wezterm",
            vec![
                "start",
                "--always-new-process",
                "--class",
                "scratchpad_notes",
                "--",
            ],
        ),
        (
            "gnome-terminal",
            vec![
                "--wait",
                "--class",
                "scratchpad_notes",
                "--title",
                "Notes with spaces",
                "--",
            ],
        ),
        (
            "st",
            vec!["-c", "scratchpad_notes", "-T", "Notes with spaces", "-e"],
        ),
    ] {
        let expected: Vec<OsString> = prefix
            .into_iter()
            .map(OsString::from)
            .chain(request.argv.iter().cloned())
            .collect();
        assert_eq!(args(&request, Path::new(program)), expected, "{program}");
    }
}

#[test]
fn alias_resolves_relative_link_chain_for_profile() {
    let dir = TestDir::new();
    symlink("foot", dir.0.join("next")).unwrap();
    symlink("next", dir.0.join("terminal")).unwrap();
    assert_eq!(
        args(&request(), &dir.0.join("terminal")),
        args(&request(), Path::new("foot"))
    );
}

#[test]
fn blank_launch_builds_no_terminal_options_even_for_missing_alias() {
    let mut request = request();
    request.argv.clear();
    request.app_id = None;
    request.title = None;
    assert!(args(&request, Path::new("/missing/default/terminal")).is_empty());
}

#[test]
fn preferred_then_alias_then_backend_candidates() {
    let mut request = request();
    request.preferred = Some("custom".into());
    request.alias = Some("/home/test/default/terminal".into());
    let programs: Vec<_> = request.programs().collect();
    assert_eq!(
        &programs[..2],
        &[
            OsStr::new("custom"),
            OsStr::new("/home/test/default/terminal")
        ]
    );
    assert!(programs.contains(&OsStr::new("foot")));
    assert!(
        !request
            .backend(Backend::X11)
            .programs()
            .any(|program| program == "foot")
    );
}

#[test]
fn only_missing_execution_triggers_fallback_and_rebuilds_options() {
    let mut request = request();
    request.preferred = Some("kitty".into());
    let mut seen = Vec::new();
    let result = request.attempt(|command| {
        seen.push(command.get_program().to_owned());
        if command.get_program() == "foot" {
            assert_eq!(command.get_args().next(), Some(OsStr::new("--app-id")));
            Ok(())
        } else {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
    });
    assert!(result.is_ok());
    assert_eq!(seen.iter().filter(|program| *program == "kitty").count(), 1);
    let mut calls = 0;
    let error = request
        .attempt::<()>(|_| {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls, 1);
}

#[test]
fn explicit_override_does_not_fall_back() {
    let request = request().terminal("missing");
    let mut calls = 0;
    assert!(
        request
            .attempt::<()>(|_| {
                calls += 1;
                Err(io::Error::from(io::ErrorKind::NotFound))
            })
            .is_err()
    );
    assert_eq!(calls, 1);
}

#[test]
fn real_missing_preference_falls_back_without_modifying_alias() {
    let dir = TestDir::new();
    let target = dir.executable("foot", "#!/bin/sh\nexit 0\n");
    let alias = dir.0.join("terminal");
    symlink(&target, &alias).unwrap();
    let mut request = request();
    request.preferred = Some(dir.0.join("absent").into_os_string());
    request.alias = Some(alias.clone());
    assert!(request.status().unwrap().success());
    assert_eq!(fs::read_link(alias).unwrap(), target);
}

#[test]
fn child_exit_127_is_not_retried() {
    let dir = TestDir::new();
    let script = dir.executable("terminal", "#!/bin/sh\nexit 127\n");
    let mut request = request();
    request.preferred = Some(script.into_os_string());
    assert_eq!(request.status().unwrap().code(), Some(127));
}

#[test]
fn atomic_set_preserves_old_alias_on_failure() {
    let dir = TestDir::new();
    let target = dir.executable("foot", "#!/bin/sh\nexit 0\n");
    let alias = dir.0.join("terminal");
    set_default(&alias, &target).unwrap();
    assert!(set_default(&alias, &dir.0.join("absent")).is_err());
    assert_eq!(fs::read_link(&alias).unwrap(), target);
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
}

#[test]
fn setup_preserves_custom_wrapper_and_live_symlink() {
    let dir = TestDir::new();
    let wrapper = dir.executable("terminal", "#!/bin/sh\nexit 0\n");
    ensure_default(&wrapper, Backend::X11).unwrap();
    assert!(
        !fs::symlink_metadata(&wrapper)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let alias = dir.0.join("alias");
    symlink(&wrapper, &alias).unwrap();
    ensure_default(&alias, Backend::X11).unwrap();
    assert_eq!(fs::read_link(alias).unwrap(), wrapper);
}

#[test]
fn cli_delimiter_preserves_command_flags_and_spaces() {
    let parsed = parse_cli(
        [
            "--terminal",
            "foot",
            "--app-id",
            "notes",
            "--",
            "sh",
            "-c",
            "echo hello world",
        ]
        .map(OsString::from),
    )
    .unwrap()
    .unwrap();
    assert_eq!(parsed.explicit, Some("foot".into()));
    assert_eq!(
        parsed.argv,
        ["sh", "-c", "echo hello world"].map(OsString::from)
    );
    assert!(parse_cli(["--title"].map(OsString::from)).is_err());
    assert!(parse_cli(["--typo"].map(OsString::from)).is_err());
}
