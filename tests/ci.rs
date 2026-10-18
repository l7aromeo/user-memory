//! The check workflow runs what a commit must pass, on both platforms, with
//! the toolchain the server image and the release builds use, and no secrets.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

use std::{fs, path::Path, process::Command};

use yaml_rust2::YamlLoader;

fn repo_file(name: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(name)).unwrap()
}

/// Every workflow file's name and text, in name order.
fn workflows() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let mut found: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read_to_string(&path).unwrap())
        })
        .collect();
    found.sort();
    found
}

/// GitHub runs no job of a workflow it cannot parse, while every text check
/// in this file would still pass; so each workflow must load as YAML, with
/// jobs.
#[test]
fn every_workflow_is_valid_yaml_with_jobs() {
    let found = workflows();
    assert!(found.len() >= 2, "only {} workflows found", found.len());
    for (name, text) in found {
        let docs = YamlLoader::load_from_str(&text).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(docs.len(), 1, "{name}: one YAML document");
        assert!(
            docs[0]["jobs"]
                .as_hash()
                .is_some_and(|jobs| !jobs.is_empty()),
            "{name}: no jobs"
        );
    }
}

/// actionlint checks what YAML alone does not: the workflow schema,
/// expressions and each step's shell. Skipped where it is not installed, as
/// the script tests skip shellcheck.
#[test]
fn actionlint_accepts_every_workflow_where_it_is_installed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let paths = workflows()
        .into_iter()
        .map(|(name, _)| root.join(".github/workflows").join(name));
    let Ok(out) = Command::new("actionlint")
        .args(paths)
        .current_dir(root)
        .output()
    else {
        return;
    };
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_check_workflow_runs_every_check_on_both_platforms_without_secrets() {
    let workflow = repo_file(".github/workflows/check.yml");
    for line in [
        "        os: [ubuntu-latest, macos-latest]",
        "      - run: just check",
        "      - run: python3 -I evals/test_run.py",
        "          key: cargo-${{ runner.os }}-${{ hashFiles('Cargo.lock') }}",
        "  contents: read",
    ] {
        assert!(workflow.lines().any(|l| l == line), "{line}");
    }
    assert!(!workflow.contains("secrets."), "{workflow}");
    // One toolchain for every build: the Dockerfile's.
    let dockerfile = repo_file("Dockerfile");
    let image = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("ARG RUST_IMAGE=rust:"))
        .unwrap();
    let version = image.split('-').next().unwrap();
    assert!(
        workflow.contains(&format!("toolchain: \"{version}\"")),
        "the workflow's toolchain is not the Dockerfile's {version}"
    );
}

/// The Dockerfile's Rust version, which every workflow builds with.
fn dockerfile_rust() -> String {
    let dockerfile = repo_file("Dockerfile");
    let image = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("ARG RUST_IMAGE=rust:"))
        .unwrap();
    image.split('-').next().unwrap().to_owned()
}

/// The docker workflow runs the opt-in Docker installer test for real on
/// Linux, with a server build made as a release makes it. A test it names
/// that no longer exists would run nothing and still pass, so the name is
/// checked against tests/install.rs.
#[test]
fn the_docker_workflow_runs_the_real_docker_installer_test() {
    let text = repo_file(".github/workflows/docker.yml");
    assert!(!text.contains("secrets."), "{text}");
    let docs = YamlLoader::load_from_str(&text).unwrap();
    let job = &docs[0]["jobs"]["docker"];
    assert_eq!(job["runs-on"].as_str(), Some("ubuntu-latest"));
    let steps = job["steps"].as_vec().unwrap();
    let step = |prefix: &str| {
        steps
            .iter()
            .find(|step| {
                step["run"]
                    .as_str()
                    .is_some_and(|run| run.starts_with(prefix))
            })
            .unwrap_or_else(|| panic!("no step runs {prefix}"))
    };
    let build = step("zsh scripts/client-release --build-only --target x86_64-unknown-linux-gnu");
    let test = step("cargo test --locked --test install ");
    assert!(
        steps.iter().position(|s| s == build) < steps.iter().position(|s| s == test),
        "the server is built before the test"
    );
    assert_eq!(
        test["env"]["USER_MEMORY_TEST_DOCKER_BINARY"].as_str(),
        Some("${{ runner.temp }}/build/user-memory-x86_64-unknown-linux-gnu")
    );
    let name = test["run"]
        .as_str()
        .unwrap()
        .strip_prefix("cargo test --locked --test install ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap();
    let tests = repo_file("tests/install.rs");
    let matching: Vec<&str> = tests
        .lines()
        .filter_map(|line| line.strip_prefix("fn "))
        .filter_map(|rest| rest.split('(').next())
        .filter(|function| function.contains(name))
        .collect();
    assert_eq!(
        matching,
        [
            "a_real_docker_server_installs_upgrades_and_uninstalls_when_opted_in",
            "a_real_docker_server_only_host_when_opted_in",
        ],
        "the workflow's filter {name} must match the real Docker tests"
    );
    assert!(
        steps
            .iter()
            .any(|s| s["with"]["toolchain"].as_str() == Some(&dockerfile_rust())),
        "the workflow's toolchain is not the Dockerfile's {}",
        dockerfile_rust()
    );
}

/// The shellcheck the check workflow installs: its latest stable release
/// when this was written (2026-10-17), checked against koalaman/shellcheck's
/// releases. The runner's preinstalled one is older and flags lines this one
/// accepts, so the version is pinned rather than taken from the image.
const SHELLCHECK: &str = "0.11.0";

/// The cargo-nextest the check workflow installs for `just check`: its
/// latest stable release when this was written (2026-10-18), checked
/// against crates.io. Pinned, so CI runs the tests with the nextest
/// developers use rather than whatever was released last.
const NEXTEST: &str = "0.9.148";

/// The PSScriptAnalyzer the check workflow installs for the install.ps1
/// tests: its latest stable release when this was written (2026-10-18),
/// checked against the PowerShell Gallery. Both runner images ship it, but
/// an image update could change the version, so it is pinned.
const PSSCRIPTANALYZER: &str = "1.25.0";

#[test]
fn the_check_workflow_runs_the_pinned_shellcheck_nextest_and_psscriptanalyzer() {
    let docs = YamlLoader::load_from_str(&repo_file(".github/workflows/check.yml")).unwrap();
    // Each step by what it runs or the tools it installs.
    let steps: Vec<&str> = docs[0]["jobs"]["check"]["steps"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|step| {
            step["run"]
                .as_str()
                .or(step["with"]["tool"].as_str())
                .unwrap_or("")
        })
        .collect();
    let at = |wanted: &str| {
        steps
            .iter()
            .position(|step| *step == wanted)
            .unwrap_or_else(|| panic!("no step {wanted:?} in {steps:?}"))
    };
    let install = at(&format!(
        "just,shellcheck@{SHELLCHECK},cargo-nextest@{NEXTEST}"
    ));
    // Fails the run if another shellcheck comes first on PATH.
    let version = at(&format!(
        "shellcheck --version | grep -Fx 'version: {SHELLCHECK}'"
    ));
    assert!(
        install < version && version < at("just check"),
        "shellcheck is not installed and checked before the checks that use it: {steps:?}"
    );
    // The install.ps1 tests need PSScriptAnalyzer in CI: installed at the pin,
    // then checked, before the checks that use it.
    let module = at(&format!(
        "pwsh -NoProfile -Command \"Install-Module PSScriptAnalyzer -RequiredVersion {PSSCRIPTANALYZER} -Scope CurrentUser -Force\""
    ));
    let present = at(&format!(
        "pwsh -NoProfile -Command \"if (-not ((Get-Module -ListAvailable PSScriptAnalyzer).Version -contains [version]'{PSSCRIPTANALYZER}')) {{ exit 1 }}\""
    ));
    assert!(
        module < present && present < at("just check"),
        "PSScriptAnalyzer is not installed and checked before the checks that use it: {steps:?}"
    );
    // `just check` runs the tests through nextest, so CI needs it installed.
    let justfile = repo_file("justfile");
    assert!(
        justfile
            .split("\ncheck:\n")
            .nth(1)
            .and_then(|recipe| recipe.split("\n\n").next())
            .is_some_and(|recipe| recipe
                .lines()
                .any(|l| l == "    cargo nextest run --locked")),
        "just check no longer runs cargo nextest; update the install step"
    );
}

/// Each action the workflows use, at the latest major release when this
/// was written (2026-10-17), checked against the action's releases. A
/// workflow on an older major, or on an action not listed, fails here, so
/// an old version cannot come back unnoticed and a new action is checked
/// before it is added. Raise a major only after reading its release notes.
const ACTIONS: [(&str, &str); 6] = [
    ("actions/checkout", "v7"),
    ("actions/cache", "v6"),
    ("actions/upload-artifact", "v7"),
    ("actions/download-artifact", "v8"),
    // Its only release; the tag follows the main branch.
    ("dtolnay/rust-toolchain", "v1"),
    ("taiki-e/install-action", "v2"),
];

/// Why a `uses:` line may stay below the action's latest major: a comment
/// on that line starting with this, then the reason, such as a minimum
/// supported version.
const PIN_REASON: &str = "# Pinned below latest:";

/// Why a workflow's `uses:` line breaks the rule, if it does: an action not
/// in ACTIONS, or one below its latest major without a reason beside it.
fn action_pin_error(line: &str) -> Option<String> {
    let used = line
        .trim()
        .trim_start_matches("- ")
        .strip_prefix("uses: ")?;
    let (action, comment) = match used.split_once(" #") {
        Some((action, comment)) => (action.trim(), format!("#{comment}")),
        None => (used.trim(), String::new()),
    };
    let (repo, version) = action.split_once('@')?;
    let Some((_, latest)) = ACTIONS.iter().find(|(listed, _)| *listed == repo) else {
        return Some(format!(
            "{repo} is not in ACTIONS; add it at its latest major"
        ));
    };
    let reason = comment
        .strip_prefix(PIN_REASON)
        .map(str::trim)
        .unwrap_or("");
    (version != *latest && reason.is_empty()).then(|| {
        format!("{repo}@{version} is below its latest major {latest}; update it, or give the reason on the line: {PIN_REASON} <reason>")
    })
}

#[test]
fn every_workflow_uses_each_actions_latest_major() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let mut used = 0;
    let mut errors = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for line in fs::read_to_string(&path).unwrap().lines() {
            if line.trim().trim_start_matches("- ").starts_with("uses: ") {
                used += 1;
            }
            if let Some(error) = action_pin_error(line) {
                errors.push(format!("{name}: {error}"));
            }
        }
    }
    assert!(
        used >= 10,
        "only {used} actions found; is the parser broken?"
    );
    assert!(errors.is_empty(), "\n{}", errors.join("\n"));
}

#[test]
fn an_action_below_its_latest_major_needs_a_reason() {
    let line = |text: &str| action_pin_error(&format!("      - uses: {text}"));
    assert_eq!(line("actions/checkout@v7"), None);
    assert!(line("actions/checkout@v4").is_some());
    assert!(line("actions/checkout@v4 # a note").is_some());
    assert!(line("actions/checkout@v4 # Pinned below latest:").is_some());
    assert_eq!(
        line("actions/checkout@v4 # Pinned below latest: the runner image needs Node 20"),
        None
    );
    assert!(line("some/new-action@v1").is_some());
}

/// The release workflow's check that its Rust toolchain is the current
/// stable, run offline against a stable channel file and a Dockerfile made
/// here.
#[cfg(unix)]
fn rust_stable(pinned_lines: &str, stable: &str) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    let channel = dir.path().join("channel-rust-stable.toml");
    fs::write(
        &channel,
        format!(
            "manifest-version = \"2\"\n\n[pkg.cargo]\nversion = \"0.100.0 (aaa 2026-10-13)\"\n\n[pkg.rust]\nversion = \"{stable} (58cf8f9c3 2026-10-13)\"\n"
        ),
    )
    .unwrap();
    let dockerfile = dir.path().join("Dockerfile");
    fs::write(
        &dockerfile,
        format!("# syntax=docker/dockerfile:1\n{pinned_lines}\nFROM ${{RUST_IMAGE}} AS build\n"),
    )
    .unwrap();
    let out = std::process::Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/check-rust-stable"))
        .env(
            "RUST_STABLE_CHANNEL",
            format!("file://{}", channel.display()),
        )
        .env("DOCKERFILE", &dockerfile)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

// The script is POSIX shell that only the release workflow's Linux job runs.
#[cfg(unix)]
#[test]
fn a_release_needs_the_current_stable_rust_or_a_reason() {
    let pin = "ARG RUST_IMAGE=rust:1.99.1-bookworm";
    let (ok, text) = rust_stable(pin, "1.99.1");
    assert!(ok, "{text}");
    // Numbers, not text: 1.100.0 is newer than 1.99.1.
    let (ok, text) = rust_stable(pin, "1.100.0");
    assert!(!ok, "{text}");
    assert!(
        text.contains("pins Rust 1.99.1, but the current stable is 1.100.0"),
        "{text}"
    );
    let (ok, text) = rust_stable(
        &format!("# Pinned below stable: our minimum supported Rust is 1.99\n{pin}"),
        "1.100.0",
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("our minimum supported Rust is 1.99"),
        "{text}"
    );
    let (ok, text) = rust_stable(&format!("# Pinned below stable:\n{pin}"), "1.100.0");
    assert!(!ok, "a reason must say something: {text}");
    let (ok, text) = rust_stable("ARG RUST_IMAGE=rust:1.100.0-bookworm", "1.99.1");
    assert!(ok, "a newer pin is fine: {text}");
}

/// The current-stable check reads the network, so only a release runs it:
/// the release workflow's version job does, the check workflow never.
#[test]
fn only_a_release_checks_rust_against_the_current_stable() {
    let release = repo_file(".github/workflows/release.yml");
    let version = release.split("\n  version:").nth(1).unwrap();
    let version = version.split("\n  build:").next().unwrap();
    assert!(
        version.contains("run: sh scripts/check-rust-stable"),
        "{version}"
    );
    assert!(!repo_file(".github/workflows/check.yml").contains("check-rust-stable"));
    assert!(!repo_file("justfile").contains("check-rust-stable"));
}

/// The paths whose change runs the Windows checks on a pull request: code,
/// tests, the build, scripts, workflows and the Windows installers. A
/// docs-only change skips them; a push to main always runs them.
const WINDOWS_PATHS: [&str; 8] = [
    "src/**",
    "tests/**",
    "Cargo.toml",
    "Cargo.lock",
    "scripts/**",
    ".github/workflows/**",
    "install.ps1",
    "uninstall.ps1",
];

#[test]
fn the_windows_workflow_checks_x64_and_arm64_on_code_changes() {
    let text = repo_file(".github/workflows/windows.yml");
    assert!(!text.contains("secrets."), "{text}");
    let docs = YamlLoader::load_from_str(&text).unwrap();
    let on = &docs[0]["on"];
    let branches = |event: &str| -> Vec<&str> {
        on[event]["branches"]
            .as_vec()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap())
            .collect()
    };
    assert_eq!(branches("push"), ["main"]);
    assert!(on["push"]["paths"].is_badvalue(), "every push to main runs");
    assert_eq!(branches("pull_request"), ["main"]);
    let paths: Vec<&str> = on["pull_request"]["paths"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(paths, WINDOWS_PATHS);
    let job = &docs[0]["jobs"]["check"];
    let runners: Vec<(&str, &str)> = job["strategy"]["matrix"]["include"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["arch"].as_str().unwrap(),
                row["runner"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        runners,
        [("x64", "windows-latest"), ("arm64", "windows-11-arm")]
    );
    assert_eq!(job["env"]["REQUIRE_PWSH"].as_str(), Some("1"));
    assert_eq!(
        job["env"]["AWS_LC_SYS_PREBUILT_NASM"].as_str(),
        Some("${{ matrix.arch == 'x64' && '1' || '' }}")
    );
    let steps: Vec<&str> = job["steps"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|step| {
            step["run"]
                .as_str()
                .or(step["with"]["tool"].as_str())
                .or(step["with"]["toolchain"].as_str())
                .unwrap_or("")
        })
        .collect();
    let at = |wanted: &str| {
        steps
            .iter()
            .position(|step| *step == wanted)
            .unwrap_or_else(|| panic!("no step {wanted:?} in {steps:?}"))
    };
    let toolchain = at(&dockerfile_rust());
    let tools = at(&format!("just,cargo-nextest@{NEXTEST}"));
    let module = at(&format!(
        "pwsh -NoProfile -Command \"Install-Module PSScriptAnalyzer -RequiredVersion {PSSCRIPTANALYZER} -Scope CurrentUser -Force\""
    ));
    let present = at(&format!(
        "pwsh -NoProfile -Command \"if (-not ((Get-Module -ListAvailable PSScriptAnalyzer).Version -contains [version]'{PSSCRIPTANALYZER}')) {{ exit 1 }}\""
    ));
    let check = at("just check");
    assert!(
        toolchain < check && tools < check && module < present && present < check,
        "the tools are installed before `just check`: {steps:?}"
    );
    // Then the install.ps1 tests under Windows PowerShell 5.1, with its own
    // PSScriptAnalyzer at the same pin.
    let module_51 = at(&format!(
        "powershell -NoProfile -Command \"[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; Install-PackageProvider NuGet -MinimumVersion 2.8.5.201 -Scope CurrentUser -Force | Out-Null; Install-Module PSScriptAnalyzer -RequiredVersion {PSSCRIPTANALYZER} -Scope CurrentUser -Force\""
    ));
    let present_51 = at(&format!(
        "powershell -NoProfile -Command \"if (-not ((Get-Module -ListAvailable PSScriptAnalyzer).Version -contains [version]'{PSSCRIPTANALYZER}')) {{ exit 1 }}\""
    ));
    let tests_51 = at("cargo nextest run --locked --test windows_install");
    assert!(
        module_51 < present_51 && present_51 < tests_51,
        "Windows PowerShell's PSScriptAnalyzer is installed and checked before its tests: {steps:?}"
    );
    let step_51 = &job["steps"].as_vec().unwrap()[tests_51];
    assert_eq!(
        step_51["env"]["USER_MEMORY_TEST_POWERSHELL"].as_str(),
        Some("powershell"),
        "the second run is Windows PowerShell's"
    );
    let tests = repo_file("tests/windows_install.rs");
    assert!(
        tests.contains("std::env::var(\"USER_MEMORY_TEST_POWERSHELL\")"),
        "tests/windows_install.rs no longer reads USER_MEMORY_TEST_POWERSHELL"
    );
    // The recipes run in Windows PowerShell there.
    let justfile = repo_file("justfile");
    assert!(
        justfile
            .lines()
            .any(|l| l == "set windows-shell := [\"powershell.exe\", \"-NoLogo\", \"-Command\"]"),
        "the justfile names no Windows shell"
    );
}

/// The release builds both Windows targets on Windows runners, only the
/// client binary, with this checkout's and the runner's paths remapped out
/// of it, and writes the checksum install.ps1 checks, in shasum's format.
#[test]
fn the_release_builds_both_windows_targets_on_windows() {
    let text = repo_file(".github/workflows/release.yml");
    let docs = YamlLoader::load_from_str(&text).unwrap();
    let job = &docs[0]["jobs"]["build"];
    let windows: Vec<(&str, &str)> = job["strategy"]["matrix"]["include"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["target"].as_str().unwrap(),
                row["runner"].as_str().unwrap(),
            )
        })
        .filter(|(target, _)| target.ends_with("-pc-windows-msvc"))
        .collect();
    assert_eq!(
        windows,
        [
            ("x86_64-pc-windows-msvc", "windows-2025"),
            ("aarch64-pc-windows-msvc", "windows-11-arm"),
        ]
    );
    let steps = job["steps"].as_vec().unwrap();
    let named = |name: &str| {
        steps
            .iter()
            .find(|step| step["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("no step {name}"))
    };
    let build = named("Build on Windows");
    assert_eq!(build["if"].as_str(), Some("runner.os == 'Windows'"));
    assert_eq!(build["shell"].as_str(), Some("pwsh"));
    let run = build["run"].as_str().unwrap();
    for line in [
        "$PSNativeCommandUseErrorActionPreference = $true",
        // MSVC and clang-cl get no path map for the C dependencies, so their
        // sources are kept out of the profile the binary is checked for.
        "$cargoHome = Join-Path $env:RUNNER_TEMP 'cargo-home'",
        "$env:CARGO_HOME = $cargoHome",
        "$env:RUSTFLAGS = \"--remap-path-prefix=$workspace=/src --remap-path-prefix=$cargoHome=/cargo --remap-path-prefix=$userProfile=/home\"",
        "cargo build --release --locked --bin user-memory --target $env:TARGET",
        // The refusal's exit code would otherwise end the step as failed.
        "$global:LASTEXITCODE = 0",
        "$name = \"user-memory-$env:TARGET.exe\"",
        "[IO.File]::WriteAllText(\"$binary.sha256\", \"$hash  $name`n\")",
    ] {
        assert!(run.lines().any(|l| l.trim() == line), "{line}\n{run}");
    }
    assert!(run.contains("Select-String -LiteralPath $binary"), "{run}");
    assert!(
        run.find("$env:CARGO_HOME = $cargoHome").unwrap()
            < run.find("cargo build --release").unwrap(),
        "the build uses the moved cargo home: {run}"
    );
    // A failed check shows what holds the path, so its cause is in the log.
    assert!(
        run.contains("$text.IndexOf($path, $at, [StringComparison]::OrdinalIgnoreCase)"),
        "{run}"
    );
    // The other builds go through the script, which refuses Windows targets.
    assert_eq!(
        named("Build")["if"].as_str(),
        Some("runner.os != 'Windows'")
    );
    assert_eq!(
        named("The binary")["if"].as_str(),
        Some("runner.os != 'Windows'")
    );
}

// CI reports every failing test in one run, through the nextest profile
// both workflows select; a local `just check` keeps the default and stops
// at the first failure.
#[test]
fn ci_runs_report_every_failure() {
    let config = repo_file(".config/nextest.toml");
    let ci = config
        .split("[profile.ci]")
        .nth(1)
        .expect("a ci profile in .config/nextest.toml");
    let ci = ci.split("\n[").next().unwrap();
    assert!(
        ci.lines().any(|line| line.trim() == "fail-fast = false"),
        "{ci}"
    );
    for (file, job) in [("check.yml", "check"), ("windows.yml", "check")] {
        let docs =
            YamlLoader::load_from_str(&repo_file(&format!(".github/workflows/{file}"))).unwrap();
        assert_eq!(
            docs[0]["jobs"][job]["env"]["NEXTEST_PROFILE"].as_str(),
            Some("ci"),
            "{file}"
        );
    }
    assert!(
        !repo_file("justfile").contains("NEXTEST_PROFILE"),
        "a local `just check` stops at the first failure"
    );
}

// A hung test ends the CI run in minutes, not at GitHub's six-hour limit: the
// ci profile reports a test as slow each minute and ends it after five. The
// slowest real test takes about a minute (a Docker install test on Ubuntu).
#[test]
fn ci_runs_end_a_hung_test() {
    let config = repo_file(".config/nextest.toml");
    let ci = config
        .split("[profile.ci]")
        .nth(1)
        .expect("a ci profile in .config/nextest.toml");
    let ci = ci.split("\n[").next().unwrap();
    assert!(
        ci.lines()
            .any(|line| line.trim() == r#"slow-timeout = { period = "60s", terminate-after = 5 }"#),
        "{ci}"
    );
}

/// Each job's timeout-minutes, which the workflows explain with the longest
/// green run each is set from.
const JOB_TIMEOUTS: [(&str, &str, i64); 7] = [
    ("check.yml", "check", 35),
    ("docker.yml", "docker", 20),
    ("release.yml", "assemble", 15),
    ("release.yml", "build", 30),
    ("release.yml", "publish", 5),
    ("release.yml", "version", 5),
    ("windows.yml", "check", 35),
];

// A job that hangs outside the tests, in a build or an install, stops at its
// own timeout rather than GitHub's six hours; a new job needs one too.
#[test]
fn every_job_has_a_timeout() {
    let mut found = Vec::new();
    for (file, text) in workflows() {
        let docs = YamlLoader::load_from_str(&text).unwrap();
        for (job, body) in docs[0]["jobs"].as_hash().unwrap() {
            let job = job.as_str().unwrap().to_owned();
            let minutes = body["timeout-minutes"]
                .as_i64()
                .unwrap_or_else(|| panic!("{file}: job {job} has no timeout-minutes"));
            found.push((file.clone(), job, minutes));
        }
    }
    found.sort();
    let expected: Vec<_> = JOB_TIMEOUTS
        .iter()
        .map(|&(file, job, minutes)| (file.to_owned(), job.to_owned(), minutes))
        .collect();
    assert_eq!(found, expected);
}

/// Commands that exist only in debug builds, such as the tests'
/// `debug-paths`, must never ship: every release build job runs its binary
/// with one and requires clap's refusal, so a missing cfg gate fails the
/// release rather than reaching users.
#[test]
fn every_release_build_refuses_the_debug_only_commands() {
    let text = repo_file(".github/workflows/release.yml");
    let docs = YamlLoader::load_from_str(&text).unwrap();
    let steps = docs[0]["jobs"]["build"]["steps"].as_vec().unwrap();
    let run = |name: &str| {
        steps
            .iter()
            .find(|step| step["name"].as_str() == Some(name))
            .and_then(|step| step["run"].as_str())
            .unwrap_or_else(|| panic!("no step {name}"))
    };
    let unix = run("The binary");
    for line in [
        "refused=$(\"$binary\" debug-paths 2>&1) && status=0 || status=$?",
        "if [ \"$status\" -ne 2 ] || ! printf '%s' \"$refused\" | grep -qF \"unrecognized subcommand 'debug-paths'\"; then",
    ] {
        assert!(unix.lines().any(|l| l.trim() == line), "{line}\n{unix}");
    }
    let windows = run("Build on Windows");
    for line in [
        "$refused = & $binary debug-paths 2>&1 | Out-String",
        "if ($status -ne 2 -or -not $refused.Contains(\"unrecognized subcommand 'debug-paths'\")) {",
    ] {
        assert!(
            windows.lines().any(|l| l.trim() == line),
            "{line}\n{windows}"
        );
    }
}

/// Whether pwsh must run rather than skip: in CI, or with REQUIRE_PWSH set,
/// as for the install.ps1 tests.
fn pwsh_required() -> bool {
    let set = |name| std::env::var(name).is_ok_and(|value| !value.is_empty() && value != "false");
    set("CI") || set("REQUIRE_PWSH")
}

// The Windows build step ends as GitHub runs it: GitHub's pwsh wrapper
// exits with $LASTEXITCODE when the script returns, so the debug-paths
// refusal (exit 2) that the step requires must not end it. Its checks run
// here, from the refusal on, on a stand-in binary that refuses as a release
// build does.
#[cfg(unix)]
#[test]
fn the_windows_build_step_ends_well_after_the_refusal_it_requires() {
    use std::os::unix::fs::PermissionsExt;
    let found = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(
        found || !pwsh_required(),
        "pwsh is required here, but it does not run"
    );
    if !found {
        eprintln!("pwsh is not installed: skipping the Windows build step's run");
        return;
    }
    let text = repo_file(".github/workflows/release.yml");
    let docs = YamlLoader::load_from_str(&text).unwrap();
    let run = docs[0]["jobs"]["build"]["steps"]
        .as_vec()
        .unwrap()
        .iter()
        .find(|step| step["name"].as_str() == Some("Build on Windows"))
        .and_then(|step| step["run"].as_str())
        .unwrap();
    let tail = &run[run
        .find("# Debug-only commands never ship")
        .expect("the refusal check")..];
    let dir = tempfile::tempdir().unwrap();
    let name = "user-memory-x86_64-pc-windows-msvc.exe";
    let binary = dir.path().join(name);
    fs::write(
        &binary,
        "#!/bin/sh\necho \"error: unrecognized subcommand '$1'\" >&2\nexit 2\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let script = dir.path().join("step.ps1");
    fs::write(
        &script,
        format!(
            "$ErrorActionPreference = 'Stop'\n\
             $PSNativeCommandUseErrorActionPreference = $true\n\
             $workspace = 'D:\\a\\user-memory\\user-memory'\n\
             $userProfile = 'C:\\Users\\runneradmin'\n\
             $name = '{name}'\n\
             $binary = '{}'\n\
             {tail}\n\
             if ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}\n",
            binary.display()
        ),
    )
    .unwrap();
    let out = Command::new("pwsh")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(". '{}'", script.display()))
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{}\n{stdout}\n{stderr}", out.status);
    let sum = fs::read_to_string(dir.path().join(format!("{name}.sha256"))).unwrap();
    let hash = sum
        .strip_suffix(&format!("  {name}\n"))
        .expect("shasum's format");
    assert!(stdout.contains(&format!("{}…", &hash[..16])), "{stdout}");
}
