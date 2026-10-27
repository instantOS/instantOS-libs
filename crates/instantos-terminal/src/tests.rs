use super::*;

// Keep script creation and process spawning together: parallel fork/exec can
// briefly inherit another test's writable script FD and produce ETXTBSY.
struct TestDir {
    path: PathBuf,
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl TestDir {
    fn new() -> Self {
        static SCRIPTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let serial = SCRIPTS.lock().unwrap_or_else(|error| error.into_inner());
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = env::temp_dir().join(format!(
            "instantos-terminal-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self {
            path,
            _serial: serial,
        }
    }
    fn executable(&self, name: &str, content: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn request() -> Launcher {
    Launcher {
        explicit: None,
        preferred: None,
        alias: None,
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
    symlink("foot", dir.path.join("next")).unwrap();
    symlink("next", dir.path.join("terminal")).unwrap();
    assert_eq!(
        args(&request(), &dir.path.join("terminal")),
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
fn preferred_then_alias_then_automatic_candidates() {
    let mut request = request();
    request.preferred = Some("custom".into());
    request.alias = Some("/home/test/default/terminal".into());
    let programs: Vec<_> = request.programs().map(|(program, _)| program).collect();
    assert_eq!(
        &programs[..2],
        &[
            OsStr::new("custom"),
            OsStr::new("/home/test/default/terminal")
        ]
    );
    assert!(programs.contains(&OsStr::new("foot")));
}

#[test]
fn only_missing_execution_triggers_fallback_and_rebuilds_options() {
    let mut request = request();
    request.preferred = Some("kitty".into());
    let mut seen = Vec::new();
    let result = request.attempt(
        |command| {
            command.env("WAYLAND_DISPLAY", "test");
        },
        |command| {
            seen.push(command.get_program().to_owned());
            if command.get_program() == "foot" {
                assert_eq!(command.get_args().next(), Some(OsStr::new("--app-id")));
                Ok(())
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        },
    );
    assert!(result.is_ok());
    assert_eq!(seen.iter().filter(|program| *program == "kitty").count(), 1);
    let mut calls = 0;
    let error = request
        .attempt::<(), ()>(
            |_| {},
            |_| {
                calls += 1;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        )
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
            .attempt::<(), ()>(
                |_| {},
                |_| {
                    calls += 1;
                    Err(io::Error::from(io::ErrorKind::NotFound))
                }
            )
            .is_err()
    );
    assert_eq!(calls, 1);
}

#[test]
fn real_missing_preference_falls_back_without_modifying_alias() {
    let dir = TestDir::new();
    let target = dir.executable("foot", "#!/bin/sh\nexit 0\n");
    let alias = dir.path.join("terminal");
    symlink(&target, &alias).unwrap();
    let mut request = request();
    request.preferred = Some(dir.path.join("absent").into_os_string());
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
    let alias = dir.path.join("terminal");
    set_default(&alias, &target).unwrap();
    assert!(set_default(&alias, &dir.path.join("absent")).is_err());
    assert_eq!(fs::read_link(&alias).unwrap(), target);
    assert_eq!(fs::read_dir(&dir.path).unwrap().count(), 2);
}

#[test]
fn setup_preserves_custom_wrapper_and_live_symlink() {
    let dir = TestDir::new();
    let wrapper = dir.executable("terminal", "#!/bin/sh\nexit 0\n");
    ensure_default(&wrapper).unwrap();
    assert!(
        !fs::symlink_metadata(&wrapper)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let alias = dir.path.join("alias");
    symlink(&wrapper, &alias).unwrap();
    ensure_default(&alias).unwrap();
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
    assert!(parse_cli(["--backend", "x11"].map(OsString::from)).is_err());
}

#[test]
fn delegated_request_round_trips_options_and_native_arguments() {
    use std::os::unix::ffi::OsStringExt;
    let original = Launcher::new("printf")
        .args([
            OsString::from("hello 'world'\n--title"),
            OsString::from_vec(vec![0xff, b'x']),
        ])
        .terminal("/custom/foot")
        .app_id("scratchpad_notes")
        .title("Title with 'quotes' and \\slashes")
        .working_directory("/directory with spaces");
    assert!(!original.cli_args().contains(&OsString::from("--backend")));
    let parsed = parse_cli(original.cli_args()).unwrap().unwrap();
    assert_eq!(parsed.argv, original.argv);
    assert_eq!(parsed.explicit, original.explicit);
    assert_eq!(parsed.app_id, original.app_id);
    assert_eq!(parsed.title, original.title);
    assert_eq!(parsed.working_directory, original.working_directory);
    let parsed = parse_cli(Launcher::default().cli_args()).unwrap().unwrap();
    assert!(parsed.argv.is_empty());
    assert!(parsed.explicit.is_none());
}

#[test]
fn spawn_with_returns_only_successful_attempt_metadata() {
    let dir = TestDir::new();
    let target = dir.executable("foot", "#!/bin/sh\nexit 0\n");
    let mut request = request();
    request.preferred = Some(dir.path.join("absent").into_os_string());
    request.alias = Some(target.clone());
    let mut attempts = 0;
    let (mut child, (program, successful_attempt)) = request
        .spawn_with(|command| {
            attempts += 1;
            (command.get_program().to_owned(), attempts)
        })
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(program, target.into_os_string());
    assert_eq!(successful_attempt, 2);
    assert_eq!(attempts, 2);
}

#[test]
fn working_directory_is_applied_without_a_shell_cd_command() {
    let dir = TestDir::new();
    let terminal = dir.executable("foot", "#!/bin/sh\n/bin/pwd\n");
    let launcher = Launcher::new("printf")
        .terminal(terminal)
        .working_directory(&dir.path);
    let (child, ()) = launcher
        .spawn_with(|command| {
            command.stdout(Stdio::piped());
        })
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("{}\n", dir.path.display()).as_bytes()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn detached_children_are_reaped_out_of_order() {
    let dir = TestDir::new();
    let slow = dir.executable(
        "slow",
        "#!/bin/sh\nwhile [ ! -e \"$2\" ]; do /bin/sleep 0.01; done\n",
    );
    let gate = dir.path.join("allow-exit");
    let fast = dir.executable("fast", "#!/bin/sh\nexit 0\n");
    let slow_pid = Launcher::new(gate.as_os_str())
        .terminal(slow)
        .spawn_detached()
        .unwrap();
    let fast_pid = Launcher::default().terminal(fast).spawn_detached().unwrap();
    let process_exists = |pid: u32| Path::new("/proc").join(pid.to_string()).exists();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_exists(fast_pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let fast_reaped = !process_exists(fast_pid);
    let slow_running = process_exists(slow_pid);
    fs::write(&gate, "exit").unwrap();
    assert!(
        fast_reaped,
        "finished child must be reaped, not left as a zombie"
    );
    assert!(
        slow_running,
        "reaping must not block behind a longer-lived child"
    );
    while process_exists(slow_pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!process_exists(slow_pid));
}

#[test]
fn fallback_uses_callback_environment_without_caching_between_launches() {
    let dir = TestDir::new();
    dir.executable("foot", "#!/bin/sh\nprintf foot\n");
    dir.executable("alacritty", "#!/bin/sh\nprintf alacritty\n");
    let launcher = request();
    for (variable, expected) in [
        (Some("WAYLAND_DISPLAY"), "foot"),
        (None, "alacritty"),
        (Some("WAYLAND_SOCKET"), "foot"),
    ] {
        let (child, program) = launcher
            .spawn_with(|command| {
                command
                    .env_clear()
                    .env("PATH", &dir.path)
                    .stdout(Stdio::piped());
                if let Some(key) = variable {
                    command.env(key, "nested-session");
                }
                command.get_program().to_owned()
            })
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(program, expected);
        assert_eq!(output.stdout, expected.as_bytes());
    }
}

#[test]
fn removed_cleared_and_empty_display_variables_exclude_foot() {
    for operation in ["remove", "clear", "empty"] {
        let mut command = Command::new("kitty");
        // This is the inherited display snapshot taken on entering fallback.
        command
            .env("WAYLAND_DISPLAY", "parent-wayland")
            .env("WAYLAND_SOCKET", "42");
        match operation {
            "remove" => {
                command
                    .env_remove("WAYLAND_DISPLAY")
                    .env_remove("WAYLAND_SOCKET");
            }
            "clear" => {
                command.env_clear();
            }
            _ => {
                command.env("WAYLAND_DISPLAY", "").env("WAYLAND_SOCKET", "");
            }
        }
        assert!(!command_candidates(&command).contains(&"foot"));
    }
}

#[test]
fn configured_choice_is_not_filtered_by_display_environment() {
    let dir = TestDir::new();
    let foot = dir.executable("foot", "#!/bin/sh\nexit 0\n");
    let mut launcher = request();
    launcher.alias = Some(foot);
    let (mut child, ()) = launcher
        .spawn_with(|command| {
            command.env_clear();
        })
        .unwrap();
    assert!(child.wait().unwrap().success());
}
