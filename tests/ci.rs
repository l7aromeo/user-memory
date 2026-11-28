//! The check workflow runs what a commit must pass, on both platforms, with
//! the toolchain the server image and the release builds use, and no secrets.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

use std::{collections::BTreeSet, fs, path::Path, process::Command};

use yaml_rust2::{Yaml, YamlLoader};

#[path = "support/pwsh.rs"]
mod pwsh;

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
    assert_eq!(changes_paths(&docs[0]), DOCKER_PATHS);
    let job = &docs[0]["jobs"]["installer"];
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
            "a_real_docker_server_issues_client_tokens_when_opted_in",
            "a_real_docker_server_retires_its_shared_file_when_opted_in",
        ],
        "the workflow's filter {name} must match the real Docker tests"
    );
}

/// The `channel` rust-toolchain.toml names: the Rust every cargo command in
/// a checkout uses, whatever the machine's default.
fn toolchain_file_rust() -> String {
    repo_file("rust-toolchain.toml")
        .lines()
        .find_map(|l| l.strip_prefix("channel = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("rust-toolchain.toml names a channel")
        .to_owned()
}

/// The workflows that build or check with an explicit toolchain step. Each
/// keeps its own `toolchain:` line, so a run never depends on how the action
/// reads rust-toolchain.toml.
const TOOLCHAIN_WORKFLOWS: [&str; 3] = ["check.yml", "docker.yml", "windows.yml"];

/// One Rust everywhere: rust-toolchain.toml's (every cargo command in a
/// checkout, and every build installs it) and each workflow's toolchain
/// step. The file overrides what a workflow installs, so if the two
/// disagreed a run would use the file's version while its log named the
/// workflow's.
#[test]
fn the_toolchain_file_and_every_workflow_name_one_rust() {
    let rust = toolchain_file_rust();
    let mut pinned = Vec::new();
    for (name, text) in workflows() {
        let docs = YamlLoader::load_from_str(&text).unwrap();
        for (_, job) in docs[0]["jobs"].as_hash().unwrap() {
            for step in job["steps"].as_vec().into_iter().flatten() {
                if let Some(toolchain) = step["with"]["toolchain"].as_str() {
                    assert_eq!(
                        toolchain, rust,
                        "{name}: its toolchain is not rust-toolchain.toml's {rust}"
                    );
                    pinned.push(name.clone());
                }
            }
        }
    }
    for name in TOOLCHAIN_WORKFLOWS {
        assert!(
            pinned.iter().any(|p| p == name),
            "{name} has no toolchain step"
        );
    }
}

/// The Docker architectures of the Linux targets a release builds, each
/// needing its own rustup-init checksum in the Dockerfile.
const LINUX_ARCHES: [(&str, &str); 2] = [
    ("x86_64-unknown-linux-gnu", "AMD64"),
    ("aarch64-unknown-linux-gnu", "ARM64"),
];

/// The Linux builds' Rust comes from Debian bookworm and rustup, never from
/// a Rust image, so a new Rust never waits for one: the Dockerfile names no
/// Rust version, reads rust-toolchain.toml, and pins rustup-init's
/// SHA-256 for every Linux target release.yml builds.
#[test]
fn the_dockerfile_installs_the_toolchain_file_s_rust_on_debian() {
    let dockerfile = repo_file("Dockerfile");
    let rust = toolchain_file_rust();
    assert!(
        !dockerfile.contains(&rust),
        "the Dockerfile names Rust {rust}"
    );
    assert!(!dockerfile.contains("FROM rust"), "{dockerfile}");
    assert!(!dockerfile.contains("RUST_IMAGE"), "{dockerfile}");
    assert!(
        dockerfile
            .lines()
            .any(|l| l == "FROM debian:bookworm AS toolchain"),
        "{dockerfile}"
    );
    assert!(
        dockerfile
            .lines()
            .any(|l| l == "COPY rust-toolchain.toml /usr/local/rust-toolchain.toml"),
        "{dockerfile}"
    );
    assert!(
        !dockerfile
            .lines()
            .any(|l| l.contains("curl ") && l.contains('|')),
        "rustup-init is downloaded and checked, never piped: {dockerfile}"
    );
    let release = repo_file(".github/workflows/release.yml");
    for (target, arch) in LINUX_ARCHES {
        assert!(release.contains(&format!("target: {target}")), "{target}");
        let sha = dockerfile
            .lines()
            .find_map(|l| l.strip_prefix(&format!("ARG RUSTUP_SHA256_{arch}=")))
            .unwrap_or_else(|| panic!("no rustup-init checksum for {arch}"));
        assert!(
            sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "{arch}: {sha}"
        );
        assert!(
            dockerfile.contains(&format!("sha256=$RUSTUP_SHA256_{arch}")),
            "{arch}"
        );
    }
    assert!(
        repo_file(".dockerignore")
            .lines()
            .any(|l| l == "!rust-toolchain.toml"),
        "the toolchain stage needs rust-toolchain.toml in its context"
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
/// stable, run offline against a stable channel file and a
/// rust-toolchain.toml made here.
#[cfg(unix)]
fn rust_stable(pinned_lines: &str, stable: &str) -> (bool, String) {
    rust_stable_after(pinned_lines, stable, "")
}

/// [`rust_stable`] with `rest` after the channel's `[pkg.rust]` section, as
/// the real manifest's other packages and targets follow it. The script
/// runs with SIGPIPE ignored, as GitHub's runners start it.
#[cfg(unix)]
fn rust_stable_after(pinned_lines: &str, stable: &str, rest: &str) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    let channel = dir.path().join("channel-rust-stable.toml");
    fs::write(
        &channel,
        format!(
            "manifest-version = \"2\"\n\n[pkg.cargo]\nversion = \"0.100.0 (aaa 2026-10-13)\"\n\n[pkg.rust]\nversion = \"{stable} (58cf8f9c3 2026-10-13)\"\n{rest}"
        ),
    )
    .unwrap();
    let toolchain = dir.path().join("rust-toolchain.toml");
    fs::write(
        &toolchain,
        format!("[toolchain]\n{pinned_lines}\ncomponents = [\"rustfmt\", \"clippy\"]\n"),
    )
    .unwrap();
    let out = std::process::Command::new("sh")
        .args(["-c", "trap '' PIPE; exec sh \"$0\""])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/check-rust-stable"))
        .env(
            "RUST_STABLE_CHANNEL",
            format!("file://{}", channel.display()),
        )
        .env("TOOLCHAIN_FILE", &toolchain)
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
    let pin = "channel = \"1.99.1\"";
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
    let (ok, text) = rust_stable("channel = \"1.100.0\"", "1.99.1");
    assert!(ok, "a newer pin is fine: {text}");
}

/// Target sections after the channel's `[pkg.rust]`: about 900 KB, past a
/// pipe's buffer (64 KiB on Linux and macOS), as the real manifest is.
#[cfg(unix)]
const MANIFEST_TARGETS: usize = 10_000;

// The script reads the stable version from the start of a large manifest.
// Stopping there left the shell's printf writing to a closed pipe: where
// SIGPIPE is ignored it printed "printf: I/O error" before the answer.
#[cfg(unix)]
#[test]
fn the_stable_check_reads_a_large_manifest_without_an_error() {
    let rest: String = (0..MANIFEST_TARGETS)
        .map(|n| {
            format!("\n[pkg.rust.target.t{n}]\navailable = true\nurl = \"https://example.invalid/rust-{n}.tar.gz\"\n")
        })
        .collect();
    let (ok, text) = rust_stable_after("channel = \"1.99.1\"", "1.99.1", &rest);
    assert!(ok, "{text}");
    assert_eq!(text, "Rust 1.99.1 is current (stable is 1.99.1).\n");
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
/// docs-only change skips them; a push to main always runs them. The
/// workflow's `changes` job decides (scripts/ci-changes).
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

/// The paths whose change runs the real Docker installer test on a pull
/// request: the installers, the server and its build, and what decides
/// that (scripts/ci-changes).
const DOCKER_PATHS: [&str; 13] = [
    "install.sh",
    "uninstall.sh",
    "Dockerfile",
    ".dockerignore",
    "rust-toolchain.toml",
    "compose*.yaml",
    "src/**",
    "tests/install.rs",
    "Cargo.toml",
    "Cargo.lock",
    "scripts/client-release",
    "scripts/ci-changes",
    ".github/workflows/docker.yml",
];

/// The patterns `workflow`'s `changes` job gives scripts/ci-changes, one per
/// line of its `PATHS`.
fn changes_paths(workflow: &Yaml) -> Vec<String> {
    let steps = workflow["jobs"]["changes"]["steps"].as_vec().unwrap();
    let step = steps
        .iter()
        .find(|step| step["run"].as_str() == Some("sh scripts/ci-changes"))
        .expect("the changes job runs scripts/ci-changes");
    assert_eq!(
        step["env"]["EVENT"].as_str(),
        Some("${{ github.event_name }}")
    );
    step["env"]["PATHS"]
        .as_str()
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The jobs `job` needs, whether written as one name or a list.
fn needs(job: &Yaml) -> BTreeSet<&str> {
    match &job["needs"] {
        Yaml::String(one) => BTreeSet::from([one.as_str()]),
        Yaml::Array(many) => many.iter().map(|need| need.as_str().unwrap()).collect(),
        _ => BTreeSet::new(),
    }
}

/// A workflow skipped by a `paths` filter leaves a check the main ruleset
/// requires pending for good (GitHub's "Troubleshooting required status
/// checks"), so no workflow filters its runs by path. One that runs its jobs
/// only for some changes decides in a `changes` job, and every other job
/// waits for it. Its last job, named after the workflow, is the check the
/// ruleset requires: it runs whatever the others did, and it needs every
/// other job, so a job added later can't be left out of what it waits for.
#[test]
fn every_workflow_that_skips_its_jobs_ends_in_one_check_to_require() {
    let mut gated = Vec::new();
    for (file, text) in workflows() {
        let docs = YamlLoader::load_from_str(&text).unwrap();
        let workflow = &docs[0];
        for event in ["push", "pull_request"] {
            for filter in ["paths", "paths-ignore"] {
                assert!(
                    workflow["on"][event][filter].is_badvalue(),
                    "{file}: on.{event}.{filter} would leave a required check pending; decide in a changes job"
                );
            }
        }
        if workflow["jobs"]["changes"].is_badvalue() {
            continue;
        }
        let name = workflow["name"].as_str().unwrap();
        let gate = &workflow["jobs"][name];
        assert!(
            !gate.is_badvalue(),
            "{file}: no job named {name}, after the workflow, to require"
        );
        assert_eq!(
            gate["if"].as_str(),
            Some("always()"),
            "{file}: {name} must run whatever the jobs before it did"
        );
        let others: BTreeSet<&str> = workflow["jobs"]
            .as_hash()
            .unwrap()
            .keys()
            .map(|key| key.as_str().unwrap())
            .filter(|key| *key != name)
            .collect();
        assert_eq!(
            needs(gate),
            others,
            "{file}: {name} must need every other job"
        );
        for job in others.iter().filter(|job| **job != "changes") {
            let job_yaml = &workflow["jobs"][*job];
            assert!(
                needs(job_yaml).contains("changes"),
                "{file}: {job} must wait for changes"
            );
            assert_eq!(
                job_yaml["if"].as_str(),
                Some("needs.changes.outputs.code == 'true'"),
                "{file}: {job} must run only when changes says so"
            );
        }
        let checked = gate["steps"][0]["run"].as_str().unwrap();
        assert!(
            checked.contains(r#"[ "$CHANGES" = success ] || exit 1"#)
                && checked.contains("success:*|skipped:false) ;;"),
            "{file}: {name} must fail unless changes succeeded and every job passed or was skipped for a change they don't cover"
        );
        gated.push(name.to_owned());
    }
    assert_eq!(gated, ["docker", "windows"]);
}

/// scripts/ci-changes on a scratch repository, its last commit on top of a
/// base as a pull request's merge commit is: what the commit changes decides,
/// with patterns matched more broadly than a workflow's paths filter, never
/// less. Unix only: the script runs on Ubuntu runners.
#[cfg(unix)]
#[test]
fn the_change_check_runs_a_workflow_for_any_change_its_paths_match() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/ci-changes");
    if let Ok(out) = Command::new("shellcheck")
        .args(["-s", "sh"])
        .arg(&script)
        .output()
    {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    // `moved` holds files in the base that the change moves, from and to.
    let decide = |event: &str, changed: &[&str], moved: &[(&str, &str)]| -> String {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "user.name=ci", "-c", "user.email=ci@example.invalid"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(repo.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        fs::write(repo.path().join("README.md"), "base\n").unwrap();
        git(&["add", "README.md"]);
        for (from, _) in moved {
            let path = repo.path().join(from);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "moved\n").unwrap();
            git(&["add", from]);
        }
        git(&["commit", "-q", "-m", "base"]);
        for (from, to) in moved {
            fs::create_dir_all(repo.path().join(to).parent().unwrap()).unwrap();
            git(&["mv", from, to]);
        }
        for file in changed {
            let path = repo.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "changed\n").unwrap();
            git(&["add", file]);
        }
        git(&["commit", "-q", "-m", "change"]);
        let out = Command::new("sh")
            .arg(&script)
            .current_dir(repo.path())
            .env("EVENT", event)
            .env("PATHS", "src/**\ncompose*.yaml\n")
            .env_remove("GITHUB_OUTPUT")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        decide("pull_request", &["docs/guide.md"], &[]),
        "code=false"
    );
    assert_eq!(
        decide(
            "pull_request",
            &["docs/guide.md", "src/memory/read.rs"],
            &[]
        ),
        "code=true"
    );
    assert_eq!(
        decide("pull_request", &["compose.server.yaml"], &[]),
        "code=true"
    );
    // GitHub's compose*.yaml stops at /; the change check doesn't.
    assert_eq!(
        decide("pull_request", &["compose/override.yaml"], &[]),
        "code=true"
    );
    // A file moved out of src/ changes src/, though only its new path is
    // outside the patterns.
    assert_eq!(
        decide("pull_request", &[], &[("src/x.rs", "docs/x.rs")]),
        "code=true"
    );
    assert_eq!(decide("push", &["docs/guide.md"], &[]), "code=true");
}

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
    assert_eq!(changes_paths(&docs[0]), WINDOWS_PATHS);
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
    let toolchain = at(&toolchain_file_rust());
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

/// The lines of a justfile recipe's body, trimmed.
fn recipe(justfile: &str, name: &str) -> Vec<String> {
    justfile
        .split(&format!("\n{name}:\n"))
        .nth(1)
        .unwrap_or_else(|| panic!("the justfile has no {name} recipe"))
        .lines()
        .take_while(|line| line.starts_with("    "))
        .map(|line| line.trim().to_owned())
        .collect()
}

// Code compiled only in debug builds can leave a variable, import or function
// unused in the release build users get, which the debug lints of `just
// check` cannot see. So wherever CI runs `just check`, the next step lints the
// release profile; `just check` itself stays as fast as it is.
#[test]
fn ci_lints_the_release_profile_right_after_every_check() {
    let justfile = repo_file("justfile");
    assert_eq!(
        recipe(&justfile, "check-release"),
        ["cargo clippy --release --locked --all-targets -- -D warnings"]
    );
    let check = recipe(&justfile, "check");
    assert!(
        check.iter().all(|line| !line.contains("--release")),
        "{check:?}"
    );
    for file in ["check.yml", "windows.yml"] {
        let docs =
            YamlLoader::load_from_str(&repo_file(&format!(".github/workflows/{file}"))).unwrap();
        let runs: Vec<&str> = docs[0]["jobs"]["check"]["steps"]
            .as_vec()
            .unwrap()
            .iter()
            .filter_map(|step| step["run"].as_str())
            .collect();
        let at = |run: &str| runs.iter().position(|&r| r == run);
        let check = at("just check").unwrap_or_else(|| panic!("{file}: no `just check`"));
        assert_eq!(
            at("just check-release"),
            Some(check + 1),
            "{file}: `just check-release` runs right after `just check`: {runs:?}"
        );
    }
}

// The delivery index's cost budgets hold on every platform the client ships
// for: each check job runs the cost check after its tests.
#[test]
fn ci_checks_the_delivery_index_s_costs_on_every_platform() {
    for file in ["check.yml", "windows.yml"] {
        let docs =
            YamlLoader::load_from_str(&repo_file(&format!(".github/workflows/{file}"))).unwrap();
        let runs: Vec<&str> = docs[0]["jobs"]["check"]["steps"]
            .as_vec()
            .unwrap()
            .iter()
            .filter_map(|step| step["run"].as_str())
            .collect();
        assert!(
            runs.contains(&"just delivery-bench --check-cost"),
            "{file}: {runs:?}"
        );
    }
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
const JOB_TIMEOUTS: [(&str, &str, i64); 11] = [
    ("check.yml", "check", 55),
    ("docker.yml", "changes", 5),
    ("docker.yml", "docker", 5),
    ("docker.yml", "installer", 20),
    ("release.yml", "assemble", 15),
    ("release.yml", "build", 30),
    ("release.yml", "publish", 5),
    ("release.yml", "version", 5),
    ("windows.yml", "changes", 5),
    ("windows.yml", "check", 75),
    ("windows.yml", "windows", 5),
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

// The Windows build step ends as GitHub runs it: GitHub's pwsh wrapper
// exits with $LASTEXITCODE when the script returns, so the debug-paths
// refusal (exit 2) that the step requires must not end it. Its checks run
// here, from the refusal on, on a stand-in binary that refuses as a release
// build does.
#[cfg(unix)]
#[test]
fn the_windows_build_step_ends_well_after_the_refusal_it_requires() {
    use std::os::unix::fs::PermissionsExt;
    if !pwsh::available("pwsh", "the Windows build step's checks") {
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
    pwsh::assert_exit(&out, 0, "the Windows build step");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let sum = fs::read_to_string(dir.path().join(format!("{name}.sha256"))).unwrap();
    let hash = sum
        .strip_suffix(&format!("  {name}\n"))
        .expect("shasum's format");
    assert!(stdout.contains(&format!("{}…", &hash[..16])), "{stdout}");
}

// THIRD_PARTY_NOTICES.txt covers what the release ships: cargo-about lists
// the dependencies of the targets about.toml names, so those must be
// exactly the targets release.yml builds, or a target's own dependencies
// (such as the Windows-only crates) ship without their notices.
#[test]
fn the_notices_cover_every_target_the_release_builds() {
    let about = repo_file("about.toml");
    let list = about
        .split_once("\ntargets = [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(list, _)| list)
        .expect("about.toml lists its targets");
    let mut noticed: Vec<String> = list
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_owned())
        .filter(|item| !item.is_empty())
        .collect();
    noticed.sort();
    let docs = YamlLoader::load_from_str(&repo_file(".github/workflows/release.yml")).unwrap();
    let mut built: Vec<String> = docs[0]["jobs"]["build"]["strategy"]["matrix"]["include"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|row| row["target"].as_str().unwrap().to_owned())
        .collect();
    built.sort();
    assert_eq!(
        noticed, built,
        "about.toml's targets are the release's builds"
    );
}

/// The tests that take `MEASURING`, as (test binary, test name): each `fn`
/// in tests/ with a `let` that takes it, as `let _alone = MEASURING`.
fn measuring_tests() -> std::collections::BTreeSet<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut found = std::collections::BTreeSet::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let binary = path.file_stem().unwrap().to_string_lossy().into_owned();
        let mut current = None;
        for line in fs::read_to_string(&path).unwrap().lines() {
            if let Some(rest) = line.strip_prefix("fn ") {
                current = rest.split('(').next().map(str::to_owned);
            }
            let code = line.trim();
            if code.starts_with("let ") && code.contains(" = MEASURING") {
                let name = current.clone().expect("MEASURING is taken inside a fn");
                found.insert((binary.clone(), name));
            }
        }
    }
    found
}

/// The names `kind(...)` holds in a nextest filter, without `=`.
fn filter_names(filter: &str, kind: &str) -> std::collections::BTreeSet<String> {
    filter
        .split(&format!("{kind}("))
        .skip(1)
        .filter_map(|rest| rest.split(')').next())
        .map(|name| name.trim_start_matches('=').to_owned())
        .collect()
}

/// `MEASURING` keeps the timing tests apart only within one process, as
/// `cargo test` runs them; nextest runs each test in a process of its own.
/// So every test that takes it is in the nextest override that runs it
/// alone, and that override names no other test.
#[test]
fn every_test_that_takes_measuring_runs_alone_under_nextest() {
    let measuring = measuring_tests();
    assert!(
        !measuring.is_empty(),
        "no test takes MEASURING; drop this check and its override"
    );
    let config = repo_file(".config/nextest.toml");
    let alone = config
        .split("[[profile.")
        .find(|table| table.contains("threads-required = \"num-test-threads\""))
        .expect(".config/nextest.toml has no override that runs tests alone");
    assert!(
        alone.starts_with("default.overrides]]"),
        "the override is in the default profile, which every profile inherits: {alone}"
    );
    assert!(
        alone.contains("success-output = \"final\""),
        "the override prints the tests' times on success too: {alone}"
    );
    let binaries: std::collections::BTreeSet<String> =
        measuring.iter().map(|(binary, _)| binary.clone()).collect();
    let names: std::collections::BTreeSet<String> =
        measuring.iter().map(|(_, name)| name.clone()).collect();
    assert_eq!(filter_names(alone, "binary"), binaries, "{alone}");
    assert_eq!(filter_names(alone, "test"), names, "{alone}");
}
