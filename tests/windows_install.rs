//! The Windows installer scripts, install.ps1 and uninstall.ps1, run under
//! PowerShell: they parse, PSScriptAnalyzer finds nothing, and the paths
//! that end before anything Windows-only (help, refusals, the build choice,
//! the checksum, the uninstaller's stand-in binary) behave as on Windows.
//! Every run points the releases address at a server of the test's own, so
//! nothing leaves this machine.
//!
//! They run under PowerShell 7 (`pwsh`), or under the PowerShell that
//! USER_MEMORY_TEST_POWERSHELL names, such as `powershell` for Windows
//! PowerShell 5.1. In CI (CI set, as GitHub sets it) or with REQUIRE_PWSH
//! set, that PowerShell and its PSScriptAnalyzer module are required and
//! their absence fails the tests; elsewhere the tests skip without them,
//! with a note.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::{TempDir, tempdir};
#[path = "support/port.rs"]
mod port;
#[path = "support/releases.rs"]
mod releases;
use port::closed_port;
use releases::{Releases, TAG};

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// VALUE as a PowerShell single-quoted string. `-Command` joins the
/// arguments after the command into the command itself, so values go into
/// the command text, not into `$args`.
fn quoted(value: &Path) -> String {
    format!("'{}'", value.display().to_string().replace('\'', "''"))
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The PowerShell the tests run: USER_MEMORY_TEST_POWERSHELL, else `pwsh`.
fn powershell() -> String {
    std::env::var("USER_MEMORY_TEST_POWERSHELL")
        .ok()
        .filter(|program| !program.is_empty())
        .unwrap_or_else(|| "pwsh".into())
}

/// Whether the checks must run rather than skip: in CI, or with
/// REQUIRE_PWSH set.
fn required() -> bool {
    let set = |name| std::env::var(name).is_ok_and(|value| !value.is_empty() && value != "false");
    set("CI") || set("REQUIRE_PWSH")
}

/// Whether the PowerShell the tests run is installed; the tests skip
/// without it, unless it is required.
fn has_powershell() -> bool {
    let program = powershell();
    let found = Command::new(&program)
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(
        found || !required(),
        "{program} is required here (CI or REQUIRE_PWSH is set), but it does not run"
    );
    if !found {
        eprintln!("{program} is not installed: skipping the Windows installer script checks");
    }
    found
}

/// A run of a script as `-File`, with no input, the local app data folder
/// in `home`, and releases fetched from `releases`.
fn run(script: &str, args: &[&str], home: &Path, releases: &str) -> Output {
    Command::new(powershell())
        .args(["-NoProfile", "-NonInteractive", "-File"])
        .arg(repo_file(script))
        .args(args)
        .env("LOCALAPPDATA", home)
        .env("USER_MEMORY_RELEASES", releases)
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN")
        .env_remove("PROCESSOR_ARCHITEW6432")
        .env("PROCESSOR_ARCHITECTURE", "AMD64")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The processor Windows itself names to the PowerShell the tests run,
/// whatever the test's environment says, as the scripts read it
/// (PROCESSOR_ARCHITEW6432, else PROCESSOR_ARCHITECTURE): Windows sets
/// PROCESSOR_ARCHITECTURE for each program from its own build, and
/// PROCESSOR_ARCHITEW6432 for an emulated one. ARM64 on GitHub's ARM64
/// runner, for a native ARM64 PowerShell and an emulated x64 one alike;
/// none where the variables pass through as set, as on x64 Windows, Linux
/// and macOS.
fn forced_machine() -> Option<String> {
    let out = Command::new(powershell())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }",
        ])
        .env_remove("PROCESSOR_ARCHITEW6432")
        .env("PROCESSOR_ARCHITECTURE", "AMD64")
        .output()
        .unwrap();
    let machine = text(&out).0.trim().to_owned();
    (machine != "AMD64").then_some(machine)
}

/// The Windows build the scripts choose in [`run`], which says AMD64: the
/// x64 one, or the ARM64 one where Windows names the ARM64 processor.
fn run_build() -> &'static str {
    if forced_machine().as_deref() == Some("ARM64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    }
}

/// The fake releases server with `files` (asset name and content) under
/// both the latest and the TAG release.
fn releases_with(files: &[(String, Vec<u8>)]) -> Releases {
    let releases = Releases::start();
    for layout in ["latest/download".to_owned(), format!("download/{TAG}")] {
        for (name, body) in files {
            fs::write(releases.asset(&layout, name), body).unwrap();
        }
    }
    releases
}

/// A releases address where nothing listens, for runs that must fail
/// before any download.
fn nowhere() -> String {
    format!("http://127.0.0.1:{}", closed_port())
}

fn home() -> TempDir {
    tempdir().unwrap()
}

#[test]
fn the_powershell_scripts_parse_and_pass_the_script_analyzer() {
    if !has_powershell() {
        return;
    }
    for name in ["install.ps1", "uninstall.ps1"] {
        let path = repo_file(name);
        let parse = Command::new(powershell())
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!(
                "$errors = $null; \
                 [void][Management.Automation.Language.Parser]::ParseFile({}, [ref]$null, [ref]$errors); \
                 foreach ($e in $errors) {{ \"$($e.Extent.StartLineNumber): $($e.Message)\" }}; \
                 exit $errors.Count",
                quoted(&path)
            ))
            .output()
            .unwrap();
        assert!(
            parse.status.success(),
            "{name} does not parse:\n{}",
            text(&parse).0
        );

        // PSScriptAnalyzer is a separate module; where it is not required
        // and missing, only the parse is checked. Its default rules apply;
        // each suppression sits in the script beside what it covers.
        let lint = Command::new(powershell())
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!(
                "if (-not (Get-Module -ListAvailable PSScriptAnalyzer)) {{ 'no PSScriptAnalyzer'; exit 255 }}; \
                 $found = @(Invoke-ScriptAnalyzer -Path {}); \
                 $found | Format-Table -AutoSize -Wrap Line, RuleName, Message | Out-String -Width 200; \
                 exit $found.Count",
                quoted(&path)
            ))
            .output()
            .unwrap();
        if lint.status.code() == Some(255) {
            assert!(
                !required(),
                "PSScriptAnalyzer is required here (CI or REQUIRE_PWSH is set), but it is not installed"
            );
            eprintln!("PSScriptAnalyzer is not installed: {name} is only parsed");
            continue;
        }
        assert!(
            lint.status.success(),
            "{name}: PSScriptAnalyzer findings:\n{}",
            text(&lint).0
        );
    }
}

#[test]
fn the_powershell_scripts_print_their_help() {
    if !has_powershell() {
        return;
    }
    let home = home();
    for (name, option) in [
        ("install.ps1", "-Url URL"),
        ("install.ps1", "-AllowedHost HOST"),
        ("uninstall.ps1", "-Purge"),
        ("uninstall.ps1", "-InstallDir DIR"),
    ] {
        let out = run(name, &["-Help"], home.path(), &nowhere());
        assert_eq!(out.status.code(), Some(0), "{name}: {}", text(&out).1);
        assert!(text(&out).0.contains(option), "{name} -Help names {option}");
    }
}

#[test]
fn install_ps1_refuses_bad_options_before_any_download() {
    if !has_powershell() {
        return;
    }
    for (args, message) in [
        (&["-Local", "vm"][..], "-Local takes docker or native"),
        (
            &["-Client", "vim"][..],
            "-Client takes auto, codex, claude or both",
        ),
        (
            &["-Url", "https://memory.example.com", "-Local", "docker"][..],
            "give either -Url or -Local, not both",
        ),
        (
            &["-ServerOnly", "-Url", "https://memory.example.com"][..],
            "-ServerOnly runs the memory server on this machine",
        ),
        (
            &["-ServerOnly", "-Local", "native"][..],
            "-ServerOnly runs the server in Docker; leave out -Local native",
        ),
        (
            &["-ServerOnly", "-Client", "codex"][..],
            "-ServerOnly connects no agent app",
        ),
        (
            &["-AllowedHost", "https://memory.example.com"][..],
            "-AllowedHost takes a host name",
        ),
        (
            &["-Bind", "0.0.0.0"][..],
            "-Bind 0.0.0.0 would serve plain HTTP on every network",
        ),
        (&["-Bind", "my-host"][..], "-Bind takes an IPv4 address"),
        (&["-Port", "70000"][..], "-Port takes a port number"),
        (&["-Port", "80a"][..], "-Port takes a port number"),
        (&["-Version", "1.0"][..], "-Version takes a release tag"),
        (
            &["-NoEmbed", "-EmbedModel", "m"][..],
            "-NoEmbed goes without",
        ),
        (
            &["-EmbedUrl", "https://api.example.com"][..],
            "-EmbedUrl needs -EmbedModel",
        ),
        (
            &[
                "-EmbedUrl",
                "https://api.example.com",
                "-EmbedModel",
                "m",
                "-PullModel",
            ][..],
            "-InstallOllama and -PullModel are for Ollama on this machine, not -EmbedUrl",
        ),
        (
            &["-EmbedKeyFile", "key"][..],
            "-EmbedKeyFile goes with -EmbedUrl",
        ),
        (
            &["-EmbedDimensions", "0", "-EmbedModel", "m"][..],
            "-EmbedDimensions takes a positive",
        ),
        (
            &["-Url", "http://memory.example.com"][..],
            "the memory server http://memory.example.com/mcp uses plain HTTP",
        ),
        (&["-Url", "ftp://memory.example.com"][..], "not an address"),
        (
            &["-Url", "https://memory.example.com", "-EmbedModel", "m"][..],
            "semantic search is set where the memory server runs",
        ),
        (
            &[
                "-Url",
                "https://memory.example.com",
                "-AllowedHost",
                "memory.example.com",
            ][..],
            "-AllowedHost and -Bind set up a shared Docker server",
        ),
        (
            &[][..],
            "no server address: pass -Url, or -Local docker|native",
        ),
    ] {
        let home = home();
        let out = run("install.ps1", args, home.path(), &nowhere());
        let (stdout, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stdout}{stderr}");
        assert!(
            stderr.contains(&format!("user-memory install: {message}")),
            "{args:?}: {stderr}"
        );
        assert!(
            !stdout.contains("Downloading"),
            "{args:?} downloads nothing: {stdout}"
        );
        assert!(
            std::fs::read_dir(home.path()).unwrap().next().is_none(),
            "{args:?} writes nothing"
        );
    }
}

#[test]
fn install_ps1_downloads_the_build_for_the_real_processor() {
    if !has_powershell() {
        return;
    }
    let releases = Releases::start();
    let server = &releases.base;
    // Where Windows itself names the ARM64 processor, the scripts must
    // choose the ARM64 build whatever the variables say.
    let forced = forced_machine();
    for (wow, machine, build) in [
        (None, "ARM64", Some("aarch64-pc-windows-msvc")),
        (None, "AMD64", Some("x86_64-pc-windows-msvc")),
        // An x64 PowerShell on ARM64 Windows.
        (Some("ARM64"), "AMD64", Some("aarch64-pc-windows-msvc")),
        (None, "x86", None),
    ] {
        let home = home();
        let mut command = Command::new(powershell());
        command
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(repo_file("install.ps1"))
            .args(["-Url", "https://memory.example.com", "-Version", TAG])
            .env("LOCALAPPDATA", home.path())
            .env("USER_MEMORY_RELEASES", server)
            .env_remove("USER_MEMORY_URL")
            .env("PROCESSOR_ARCHITECTURE", machine)
            .stdin(Stdio::null());
        match wow {
            Some(wow) => command.env("PROCESSOR_ARCHITEW6432", wow),
            None => command.env_remove("PROCESSOR_ARCHITEW6432"),
        };
        let out = command.output().unwrap();
        let (stdout, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(1), "{machine}: {stdout}{stderr}");
        let build = if forced.as_deref() == Some("ARM64") {
            Some("aarch64-pc-windows-msvc")
        } else {
            build
        };
        match build {
            Some(build) => assert!(
                stderr.contains(&format!(
                    "user-memory install: cannot download {server}/download/{TAG}/user-memory-{build}.exe"
                )),
                "{machine}/{wow:?}: {stderr}"
            ),
            None => assert!(
                stderr.contains("user-memory install: there is no client build for Windows on x86"),
                "{stderr}"
            ),
        }
    }
}

#[test]
fn install_ps1_installs_nothing_that_fails_its_checksum() {
    if !has_powershell() {
        return;
    }
    let build = &format!("user-memory-{}.exe", run_build());
    // Text, saved without an execute bit: no system starts it (Windows finds
    // no program in it, Linux and macOS no permission to run it), so the
    // scripts' start check fails the same way everywhere.
    let body = b"not a real build".to_vec();
    let right = {
        let out = Command::new(powershell())
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg("$s = [Security.Cryptography.SHA256]::Create(); ([BitConverter]::ToString($s.ComputeHash([Text.Encoding]::UTF8.GetBytes('not a real build'))) -replace '-', '').ToLowerInvariant()")
            .output()
            .unwrap();
        text(&out).0.trim().to_owned()
    };
    assert_eq!(right.len(), 64, "{right}");
    for (checksum, message) in [
        (
            format!("{} *{build}\n", "0".repeat(64)),
            "the download does not match its checksum; nothing was installed",
        ),
        (
            "not a checksum\n".to_owned(),
            "the release's checksum file is malformed; nothing was installed",
        ),
        // A matching build is run once to check that Windows starts it,
        // which this one (no Windows program) fails.
        (
            format!("{right}  {build}\n"),
            "Windows did not run the downloaded user-memory.exe",
        ),
    ] {
        let releases = releases_with(&[
            (build.to_owned(), body.clone()),
            (format!("{build}.sha256"), checksum.into_bytes()),
        ]);
        let home = home();
        let out = run(
            "install.ps1",
            &["-Url", "https://memory.example.com"],
            home.path(),
            &releases.base,
        );
        let (stdout, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
        assert!(
            stderr.contains(&format!("user-memory install: {message}")),
            "{stderr}"
        );
        assert!(
            !home.path().join("Programs").exists(),
            "nothing is installed: {stdout}"
        );
    }
}

/// An earlier install whose binary no longer runs is no saved setup: the
/// installer goes on as for a new one (here, to a download that fails)
/// rather than stopping at the exit code a program that never ran left
/// unset.
#[test]
fn install_ps1_goes_on_past_an_installed_binary_that_does_not_run() {
    if !has_powershell() {
        return;
    }
    let home = home();
    let bin = home.path().join("Programs/user-memory/bin");
    fs::create_dir_all(&bin).unwrap();
    // As the builds above: no system starts it.
    fs::write(bin.join("user-memory.exe"), "not a program").unwrap();
    let out = run(
        "install.ps1",
        &["-Url", "https://memory.example.com"],
        home.path(),
        &nowhere(),
    );
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stderr.contains("user-memory install: cannot download "),
        "{stdout}{stderr}"
    );
}

#[test]
fn install_ps1_exits_only_where_exit_closes_no_console() {
    if !has_powershell() {
        return;
    }
    let home = home();
    // Run as `& ([scriptblock]::Create((irm …)))` by a -Command host, which
    // ends after its command anyway: exit status 1.
    let out = Command::new(powershell())
        .args(["-NoProfile", "-Command"])
        .arg(format!(
            "& ([scriptblock]::Create((Get-Content -Raw {}))) -Local vm",
            quoted(&repo_file("install.ps1"))
        ))
        .env("LOCALAPPDATA", home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out).1);
    assert!(
        text(&out)
            .1
            .contains("user-memory install: -Local takes docker or native")
    );
    // The same with -NoExit, as a console the user pasted into: the
    // console stays, with the failure in LASTEXITCODE.
    let out = Command::new(powershell())
        .args(["-NoProfile", "-NoExit", "-Command"])
        .arg(format!(
            "& ([scriptblock]::Create((Get-Content -Raw {}))) -Local vm; \"still here $LASTEXITCODE\"; exit 0",
            quoted(&repo_file("install.ps1"))
        ))
        .env("LOCALAPPDATA", home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out).1);
    assert!(text(&out).0.contains("still here 1"), "{}", text(&out).0);
}

#[test]
fn uninstall_ps1_refuses_bad_options_before_any_change() {
    if !has_powershell() {
        return;
    }
    for (args, message) in [
        (&["-Yes"][..], "-Yes goes with -Purge"),
        (&["-NoExport"][..], "-NoExport goes with -Purge"),
        (
            &["-Client", "vim"][..],
            "-Client takes auto, codex, claude or both",
        ),
        (
            &["-Purge"][..],
            "-Purge deletes your memories; without a console to confirm, it also needs -Yes. Nothing was changed.",
        ),
    ] {
        let home = home();
        let out = run("uninstall.ps1", args, home.path(), &nowhere());
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out).1);
        assert!(
            text(&out)
                .1
                .contains(&format!("user-memory uninstall: {message}")),
            "{args:?}: {}",
            text(&out).1
        );
    }
    // Nothing installed: nothing to remove, and a clean exit.
    let home = home();
    let out = run("uninstall.ps1", &["-DryRun"], home.path(), &nowhere());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out).1);
    assert!(
        text(&out).0.contains("Nothing to remove"),
        "{}",
        text(&out).0
    );
}

#[test]
fn uninstall_ps1_falls_back_to_a_verified_release_binary() {
    if !has_powershell() {
        return;
    }
    let build = &format!("user-memory-{}.exe", run_build());
    let body = b"not a real build".to_vec();
    let mismatched = releases_with(&[
        (build.to_owned(), body),
        (
            format!("{build}.sha256"),
            format!("{} *{build}\n", "0".repeat(64)).into_bytes(),
        ),
    ]);
    for (server, reason) in [
        (nowhere(), "cannot download "),
        (
            mismatched.base.clone(),
            "the download does not match its checksum",
        ),
    ] {
        for broken in [false, true] {
            // An earlier install left its data folder, and no binary or one
            // that does not start.
            let home = home();
            fs::create_dir(home.path().join("user-memory")).unwrap();
            if broken {
                let bin = home.path().join("Programs/user-memory/bin");
                fs::create_dir_all(&bin).unwrap();
                // As the downloaded build above: no system starts it.
                fs::write(bin.join("user-memory.exe"), "not a program").unwrap();
            }
            let out = run("uninstall.ps1", &[], home.path(), &server);
            let (stdout, stderr) = text(&out);
            assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
            assert!(
                stdout.contains("using the latest release to remove what is left"),
                "{stdout}"
            );
            assert!(
                stdout.contains("  Left: what user-memory set up"),
                "{stdout}"
            );
            assert!(stdout.contains(reason), "{reason}: {stdout}");
            assert!(stdout.contains("(kept)"), "the memories are kept: {stdout}");
            assert!(
                home.path().join("user-memory").exists(),
                "nothing is removed"
            );
        }
    }
}
