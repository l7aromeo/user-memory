#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]
#![cfg(unix)]

//! `user-memory project-id`, `--set` and the global git ignore it keeps, run
//! as a user runs them, each in its own home folder.

use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{TempDir, tempdir};

const FILE: &str = ".user-memory-key";
const LIST: &str = ".user-memory";
const ADDED: &str = "# user-memory\n.user-memory\n.user-memory-key\n";

struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Self {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("home/work")).unwrap();
        Self { root }
    }
    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }
    fn path(&self, relative: &str) -> PathBuf {
        self.home().join(relative)
    }
    /// The binary in `dir` with only this home folder's git configuration:
    /// never the user's global or system git config, nor an XDG folder.
    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
        command
            .args(args)
            .current_dir(dir)
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("USER_MEMORY_URL")
            .env_remove("USER_MEMORY_DB");
        command
    }
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=t@example.invalid"])
            .args(args)
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", text(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.path(name);
        fs::create_dir_all(&dir).unwrap();
        self.git(&dir, &["init", "-q", "-b", "main"]);
        dir
    }
    /// The ignore file git uses when core.excludesFile is unset.
    fn default_ignore(&self) -> PathBuf {
        self.path(".config/git/ignore")
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn succeeded(out: &Output) {
    assert!(out.status.success(), "{}", text(out));
}

#[test]
fn project_id_names_a_folder_outside_git_by_its_parent_file() {
    let h = Home::new();
    let deep = h.path("work/app/src");
    fs::create_dir_all(&deep).unwrap();
    fs::write(h.path("work/app").join(FILE), "github.com/Me/App\n").unwrap();
    let out = h.run(&deep, &["project-id"]);
    succeeded(&out);
    assert_eq!(stdout(&out), "github.com/me/app");
}

#[test]
fn project_id_ignores_a_file_in_the_home_folder() {
    let h = Home::new();
    fs::write(h.home().join(FILE), "everything\n").unwrap();
    let out = h.run(&h.path("work"), &["project-id"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("no .user-memory"), "{}", text(&out));
}

#[test]
fn project_id_finds_the_file_without_git_installed() {
    let h = Home::new();
    let app = h.path("work/app");
    fs::create_dir_all(&app).unwrap();
    fs::write(app.join(FILE), "acme app\n").unwrap();
    let empty = h.root.path().join("no-git");
    fs::create_dir_all(&empty).unwrap();
    let out = h
        .command(&app, &["project-id"])
        .env("PATH", &empty)
        .output()
        .unwrap();
    succeeded(&out);
    assert_eq!(stdout(&out), "acme app");
}

#[test]
fn project_id_fails_when_git_is_there_but_cannot_run() {
    let h = Home::new();
    let app = h.path("work/app");
    fs::create_dir_all(&app).unwrap();
    fs::write(app.join(FILE), "acme app\n").unwrap();
    // A git that is not executable: whether the folder is in a repository
    // is unknown, so no file outside git may answer for it.
    let broken = h.root.path().join("broken-git");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("git"), "not a program").unwrap();
    let out = h
        .command(&app, &["project-id"])
        .env("PATH", &broken)
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("Cannot run git"), "{}", text(&out));
}

#[test]
fn set_writes_the_normalised_key_in_the_folder_and_prints_it() {
    let h = Home::new();
    let app = h.path("work/new-app");
    fs::create_dir_all(&app).unwrap();
    let out = h.run(
        &app,
        &["project-id", "--set", "https://GitHub.com/Me/New-App.git"],
    );
    succeeded(&out);
    assert_eq!(stdout(&out), "github.com/me/new-app");
    assert_eq!(
        fs::read_to_string(app.join(FILE)).unwrap(),
        "github.com/me/new-app\n"
    );
    let out = h.run(&app, &["project-id"]);
    assert_eq!(stdout(&out), "github.com/me/new-app");
    // The same key again changes nothing.
    let out = h.run(&app, &["project-id", "--set", "github.com/me/new-app"]);
    succeeded(&out);
    assert_eq!(stdout(&out), "github.com/me/new-app");
}

#[test]
fn set_takes_a_folder_and_writes_at_the_git_root_inside_a_repository() {
    let h = Home::new();
    let repo = h.repo("work/repo");
    let sub = repo.join("src");
    fs::create_dir_all(&sub).unwrap();
    let elsewhere = h.path("work");
    let out = h.run(
        &elsewhere,
        &[
            "project-id",
            "--set",
            "acme/platform",
            sub.to_str().unwrap(),
        ],
    );
    succeeded(&out);
    assert_eq!(stdout(&out), "acme/platform");
    assert!(repo.join(FILE).is_file());
    assert!(!sub.join(FILE).exists());
    assert_eq!(stdout(&h.run(&sub, &["project-id"])), "acme/platform");
}

#[test]
fn set_keeps_comments_and_leaves_the_list_file_alone() {
    let h = Home::new();
    let app = h.path("work/chutao");
    fs::create_dir_all(&app).unwrap();
    fs::write(app.join(FILE), "# planned remote\n").unwrap();
    fs::write(app.join(LIST), "@chutao-djs\n").unwrap();
    let out = h.run(&app, &["project-id", "--set", "github.com/me/chutao"]);
    succeeded(&out);
    assert_eq!(
        fs::read_to_string(app.join(FILE)).unwrap(),
        "github.com/me/chutao\n# planned remote\n"
    );
    assert_eq!(fs::read_to_string(app.join(LIST)).unwrap(), "@chutao-djs\n");
    assert_eq!(
        stdout(&h.run(&app, &["project-id"])),
        "github.com/me/chutao"
    );
}

#[test]
fn project_id_names_the_remote_and_the_move_a_differing_key_line_waits_for() {
    let h = Home::new();
    let repo = h.repo("work/app");
    h.git(
        &repo,
        &["remote", "add", "origin", "https://github.com/me/app"],
    );
    fs::write(repo.join(FILE), "local:app\n").unwrap();
    let out = h.run(&repo, &["project-id"]);
    succeeded(&out);
    assert_eq!(stdout(&out), "github.com/me/app");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("user-memory project move local:app github.com/me/app"),
        "{}",
        text(&out)
    );
}

#[test]
fn set_inside_a_repository_with_a_remote_accepts_only_the_remotes_key() {
    let h = Home::new();
    let repo = h.repo("work/app");
    h.git(
        &repo,
        &["remote", "add", "origin", "git@github.com:Me/App.git"],
    );
    let out = h.run(&repo, &["project-id", "--set", "acme"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("remote already names github.com/me/app"),
        "{}",
        text(&out)
    );
    let out = h.run(&repo, &["project-id", "--set", "https://github.com/me/app"]);
    succeeded(&out);
    assert_eq!(stdout(&out), "github.com/me/app");
    assert!(!repo.join(FILE).exists());
}

#[test]
fn set_refuses_to_replace_a_different_key() {
    let h = Home::new();
    let app = h.path("work/app");
    fs::create_dir_all(&app).unwrap();
    fs::write(app.join(FILE), "\nfirst key\n").unwrap();
    let out = h.run(&app, &["project-id", "--set", "second key"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("already names first key; remove it first to change the project"),
        "{}",
        text(&out)
    );
    assert_eq!(fs::read_to_string(app.join(FILE)).unwrap(), "\nfirst key\n");
}

#[test]
fn set_refuses_path_keys() {
    let h = Home::new();
    let app = h.path("work/app");
    fs::create_dir_all(&app).unwrap();
    for key in [
        "/srv/app", "~/app", "./app", ".hidden", "C:\\app", " ", "@app",
    ] {
        let out = h.run(&app, &["project-id", "--set", key]);
        assert!(!out.status.success(), "{key}: {}", text(&out));
        assert!(!app.join(FILE).exists(), "{key}");
    }
    let out = h.run(&app, &["project-id", "--set", "/srv/app"]);
    assert!(text(&out).contains("is a local path"), "{}", text(&out));
}

#[test]
fn set_refuses_the_home_folder_where_the_file_would_be_ignored() {
    let h = Home::new();
    let out = h.run(&h.home(), &["project-id", "--set", "acme"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("home folder"), "{}", text(&out));
    assert!(!h.home().join(FILE).exists());
}

#[test]
fn set_refuses_to_write_through_a_symlinked_file() {
    let h = Home::new();
    let app = h.path("work/app");
    fs::create_dir_all(&app).unwrap();
    let target = h.path("work/target");
    fs::write(&target, "").unwrap();
    symlink(&target, app.join(FILE)).unwrap();
    let out = h.run(&app, &["project-id", "--set", "acme"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("symbolic link"), "{}", text(&out));
    assert_eq!(fs::read_to_string(&target).unwrap(), "");
}

// ─── The global git ignore ──────────────────────────────────────────────────

/// Runs `project-id --set` in a fresh folder and returns its output.
fn set_in_new_folder(h: &Home, name: &str) -> Output {
    let app = h.path(&format!("work/{name}"));
    fs::create_dir_all(&app).unwrap();
    let out = h.run(&app, &["project-id", "--set", name]);
    succeeded(&out);
    out
}

/// Whether git, as this home folder configures it, ignores the file in `repo`.
fn ignored_in(h: &Home, repo: &Path) -> bool {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", FILE])
        .env("HOME", h.home())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    let list = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", LIST])
        .env("HOME", h.home())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap();
    out.status.success() && list.status.success()
}

#[test]
fn set_adds_the_file_to_the_default_ignore_once() {
    let h = Home::new();
    let repo = h.repo("work/check");
    assert!(!ignored_in(&h, &repo));
    let out = set_in_new_folder(&h, "one");
    assert!(
        text(&out).contains(&h.default_ignore().display().to_string()),
        "{}",
        text(&out)
    );
    assert_eq!(fs::read_to_string(h.default_ignore()).unwrap(), ADDED);
    assert!(ignored_in(&h, &repo));
    set_in_new_folder(&h, "two");
    assert_eq!(fs::read_to_string(h.default_ignore()).unwrap(), ADDED);
}

#[test]
fn set_appends_after_existing_lines_of_an_excludes_file_named_with_a_tilde() {
    let h = Home::new();
    fs::write(
        h.path(".gitconfig"),
        "[core]\n\texcludesFile = ~/.gitignore_global\n",
    )
    .unwrap();
    let file = h.path(".gitignore_global");
    fs::write(&file, ".DS_Store\n*.swp").unwrap();
    set_in_new_folder(&h, "one");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        format!(".DS_Store\n*.swp\n{ADDED}")
    );
    assert!(!h.default_ignore().exists());
    assert!(ignored_in(&h, &h.repo("work/check")));
}

#[test]
fn set_uses_the_xdg_config_folder_when_it_is_set() {
    let h = Home::new();
    let xdg = h.path("xdg");
    let app = h.path("work/app");
    fs::create_dir_all(&app).unwrap();
    let out = h
        .command(&app, &["project-id", "--set", "acme"])
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    succeeded(&out);
    assert_eq!(fs::read_to_string(xdg.join("git/ignore")).unwrap(), ADDED);
    assert!(!h.default_ignore().exists());
}

#[test]
fn set_leaves_a_file_that_already_ignores_both_names() {
    let h = Home::new();
    fs::create_dir_all(h.path(".config/git")).unwrap();
    for existing in [
        ".user-memory\n.user-memory-key\n",
        "/.user-memory*\n",
        ".*\n",
    ] {
        fs::write(h.default_ignore(), existing).unwrap();
        let out = set_in_new_folder(&h, &format!("app{}", existing.len()));
        assert!(text(&out).contains("already ignores"), "{}", text(&out));
        assert_eq!(fs::read_to_string(h.default_ignore()).unwrap(), existing);
    }
}

#[test]
fn set_adds_only_the_name_git_does_not_ignore_yet() {
    let h = Home::new();
    fs::create_dir_all(h.path(".config/git")).unwrap();
    for (existing, added) in [
        // An ignore from before the key file had its own name.
        ("# user-memory\n.user-memory\n", ".user-memory-key\n"),
        ("*.log\n.user-memory-key\n", "# user-memory\n.user-memory\n"),
    ] {
        fs::write(h.default_ignore(), existing).unwrap();
        let out = set_in_new_folder(&h, &format!("app{}", existing.len()));
        assert_eq!(
            fs::read_to_string(h.default_ignore()).unwrap(),
            format!("{existing}{added}"),
            "{}",
            text(&out)
        );
    }
}

#[test]
fn set_prints_the_line_for_a_symlinked_ignore_file_and_still_succeeds() {
    let h = Home::new();
    let dotfiles = h.path("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    fs::write(dotfiles.join("ignore"), "*.log\n").unwrap();
    fs::create_dir_all(h.path(".config/git")).unwrap();
    symlink(dotfiles.join("ignore"), h.default_ignore()).unwrap();
    let out = set_in_new_folder(&h, "one");
    assert!(
        text(&out).contains(&format!(
            "add .user-memory and .user-memory-key to {} yourself, one per line",
            h.default_ignore().display()
        )),
        "{}",
        text(&out)
    );
    assert_eq!(
        fs::read_to_string(dotfiles.join("ignore")).unwrap(),
        "*.log\n"
    );
}

#[test]
fn set_never_writes_through_a_symlinked_config_folder() {
    let h = Home::new();
    let dotfiles = h.path("dotfiles/config");
    fs::create_dir_all(&dotfiles).unwrap();
    symlink(&dotfiles, h.path(".config")).unwrap();
    let out = set_in_new_folder(&h, "one");
    assert!(text(&out).contains("symbolic link"), "{}", text(&out));
    assert!(!dotfiles.join("git").exists());
}

#[test]
fn set_skips_the_ignore_with_a_note_when_git_is_missing() {
    let h = Home::new();
    let app = h.path("work/app");
    fs::create_dir_all(&app).unwrap();
    let empty = h.root.path().join("no-git");
    fs::create_dir_all(&empty).unwrap();
    let out = h
        .command(&app, &["project-id", "--set", "acme"])
        .env("PATH", &empty)
        .output()
        .unwrap();
    succeeded(&out);
    assert_eq!(stdout(&out), "acme");
    assert!(text(&out).contains("git not found"), "{}", text(&out));
    assert!(!h.default_ignore().exists());
}

#[test]
fn set_never_reads_an_empty_excludes_setting_as_unset() {
    let h = Home::new();
    // Set but empty: git reads no ignore file at all, not the XDG one.
    fs::write(h.path(".gitconfig"), "[core]\n\texcludesFile =\n").unwrap();
    let out = set_in_new_folder(&h, "one");
    assert!(
        text(&out).contains("set to an empty value"),
        "{}",
        text(&out)
    );
    assert!(!h.default_ignore().exists());
}

#[test]
fn set_leaves_an_ignore_file_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new();
    let file = h.path("write-only-ignore");
    fs::write(&file, "*.log\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o200)).unwrap();
    fs::write(
        h.path(".gitconfig"),
        format!("[core]\n\texcludesFile = {}\n", file.display()),
    )
    .unwrap();
    let out = set_in_new_folder(&h, "one");
    assert!(text(&out).contains("Cannot read"), "{}", text(&out));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "*.log\n");
}

// ─── project move ───────────────────────────────────────────────────────────

/// A store in this home folder that the CLI records into.
struct Store<'a> {
    h: &'a Home,
    db: PathBuf,
}

impl<'a> Store<'a> {
    fn new(h: &'a Home) -> Self {
        Self {
            h,
            db: h.path("memory.sqlite3"),
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        self.h
            .command(&self.h.home(), args)
            .env("USER_MEMORY_DB", &self.db)
            .env_remove("USER_MEMORY_TOKEN_FILE")
            .output()
            .unwrap()
    }
    /// Records a memory and returns its id.
    fn record(&self, content: &str, extra: &[&str]) -> String {
        let kind = if extra.contains(&"--type") {
            &[][..]
        } else {
            &["--type", "fact"][..]
        };
        let mut args = vec!["record"];
        args.extend_from_slice(kind);
        args.extend_from_slice(&[
            "--category",
            "work",
            "--title",
            content,
            "--content",
            content,
        ]);
        args.extend_from_slice(extra);
        let out = self.run(&args);
        succeeded(&out);
        let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(reply["stored"], true, "{reply}");
        reply["id"].as_str().unwrap().to_owned()
    }
    fn in_project(&self, content: &str, project: &str, origin: &str) -> String {
        self.record(
            content,
            &[
                "--scope",
                "project",
                "--project",
                project,
                "--origin",
                origin,
            ],
        )
    }
    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.db).unwrap()
    }
    fn link(&self, origin: &str, project: &str) {
        self.conn()
            .execute(
                "INSERT INTO conversation_projects(origin, project) VALUES (?1, ?2)",
                [origin, project],
            )
            .unwrap();
    }
    fn links(&self) -> Vec<(String, String)> {
        self.conn()
            .prepare("SELECT origin, project FROM conversation_projects ORDER BY origin, project")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    /// (project, status, updated_at) of a memory.
    fn memory(&self, id: &str) -> (String, String, String) {
        self.conn()
            .query_row(
                "SELECT project, status, updated_at FROM memories WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }
    fn backups(&self) -> Vec<PathBuf> {
        fs::read_dir(self.h.home())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.to_string_lossy().contains("memory.sqlite3.move-"))
            .collect()
    }
}

#[test]
fn move_re_keys_project_memories_and_links_after_a_backup() {
    let h = Home::new();
    let s = Store::new(&h);
    let old = "github.com/me/old";
    let a = s.in_project("The build uses cargo nextest", old, "claude:session/a");
    let b = s.in_project("Releases are cut on Fridays", old, "codex:thread/b");
    let pattern = s.record(
        "Keeps the release process predictable",
        &[
            "--type",
            "pattern",
            "--source",
            "inferred",
            "--scope",
            "project",
            "--project",
            old,
            "--evidence-id",
            &a,
            "--evidence-id",
            &b,
        ],
    );
    let global = s.record("Prefers short answers", &["--scope", "global"]);
    let other = s.in_project(
        "Uses Postgres 17",
        "github.com/me/other",
        "claude:session/c",
    );
    s.link("claude:session/one", old);
    s.link("claude:session/two", old);
    // Already linked to the new key too: the move must not trip the key.
    s.link("claude:session/two", "github.com/me/new");
    s.link("claude:session/three", "github.com/me/other");
    let pattern_before = s.memory(&pattern);
    let rows_before: i64 = s
        .conn()
        .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    // The move is the day's first write when no automatic backup is due yet.
    let automatic = s.db.with_file_name("backups");
    fs::remove_dir_all(&automatic).unwrap();

    let out = s.run(&[
        "project",
        "move",
        "https://GitHub.com/Me/Old.git",
        "github.com/me/new",
    ]);
    succeeded(&out);
    let said = text(&out);
    assert!(
        said.contains(
            "Moved 3 memories and 2 conversation links from github.com/me/old to github.com/me/new"
        ),
        "{said}"
    );
    for id in [&a, &b, &pattern] {
        assert_eq!(s.memory(id).0, "github.com/me/new", "{id}");
    }
    // A re-key is no new evidence: the pattern keeps its status.
    assert_eq!(s.memory(&pattern).1, pattern_before.1);
    assert_eq!(s.memory(&global).0, "");
    assert_eq!(s.memory(&other).0, "github.com/me/other");
    assert_eq!(
        s.links(),
        [
            ("claude:session/one".into(), "github.com/me/new".into()),
            ("claude:session/three".into(), "github.com/me/other".into()),
            ("claude:session/two".into(), "github.com/me/new".into()),
        ]
    );
    let backups = s.backups();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert!(said.contains(&backups[0].display().to_string()), "{said}");
    let copy = rusqlite::Connection::open(&backups[0]).unwrap();
    let (kept, under_old): (i64, i64) = copy
        .query_row(
            "SELECT COUNT(*), SUM(project = ?1) FROM memories",
            [old],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((kept, under_old), (rows_before, 3));
    assert!(!said.contains("review --project"), "{said}");
    assert_eq!(fs::read_dir(&automatic).unwrap().count(), 1);
}

#[test]
fn move_suggests_a_review_when_the_new_key_already_has_memories() {
    let h = Home::new();
    let s = Store::new(&h);
    s.in_project("The build uses cargo nextest", "old", "claude:session/a");
    s.in_project("Deploys run on Mondays", "new", "claude:session/b");
    let out = s.run(&["project", "move", "old", "new"]);
    succeeded(&out);
    assert!(
        text(&out).contains("user-memory review --project new"),
        "{}",
        text(&out)
    );
}

#[test]
fn move_moves_links_alone_when_the_old_key_has_no_memories() {
    let h = Home::new();
    let s = Store::new(&h);
    s.record("Prefers short answers", &["--scope", "global"]);
    s.link("claude:session/one", "old");
    let out = s.run(&["project", "move", "old", "new"]);
    succeeded(&out);
    assert!(
        text(&out).contains("Moved 0 memories and 1 conversation link from old to new"),
        "{}",
        text(&out)
    );
    assert_eq!(s.links(), [("claude:session/one".into(), "new".into())]);
}

#[test]
fn move_refuses_without_changing_or_backing_up_anything() {
    let h = Home::new();
    let s = Store::new(&h);
    s.in_project(
        "The build uses cargo nextest",
        "github.com/me/app",
        "claude:session/a",
    );
    let before = fs::read(&s.db).unwrap();
    for (old, new, why) in [
        (
            "github.com/me/app",
            "https://github.com/Me/App.git",
            "the same project",
        ),
        ("github.com/me/app", "/srv/app", "is a local path"),
        ("~/app", "github.com/me/new", "is a local path"),
        (
            "github.com/me/none",
            "github.com/me/new",
            "No memories or conversation links use the project",
        ),
    ] {
        let out = s.run(&["project", "move", old, new]);
        assert!(!out.status.success(), "{old} {new}: {}", text(&out));
        assert!(text(&out).contains(why), "{old} {new}: {}", text(&out));
    }
    assert_eq!(fs::read(&s.db).unwrap(), before);
    assert!(s.backups().is_empty());
}

#[test]
fn move_runs_only_where_the_store_is() {
    let h = Home::new();
    let token = h.path("token");
    fs::write(&token, "x".repeat(40)).unwrap();
    let out = h
        .command(&h.home(), &["project", "move", "old", "new"])
        .env("USER_MEMORY_URL", "https://memory.example.invalid/mcp")
        .env("USER_MEMORY_TOKEN_FILE", &token)
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("docker compose exec memory user-memory project move old new"),
        "{}",
        text(&out)
    );
}

#[test]
fn move_still_stales_a_pattern_left_in_another_project() {
    let h = Home::new();
    let s = Store::new(&h);
    let a = s.in_project("The build uses cargo nextest", "old", "claude:session/a");
    let b = s.in_project("Releases are cut on Fridays", "old", "codex:thread/b");
    let pattern = s.record(
        "Keeps the release process predictable",
        &[
            "--type",
            "pattern",
            "--source",
            "inferred",
            "--scope",
            "project",
            "--project",
            "old",
            "--evidence-id",
            &a,
            "--evidence-id",
            &b,
        ],
    );
    // Moved out of the old project before the move, as a store edited by
    // hand or by an earlier version could hold it.
    s.conn()
        .execute(
            "UPDATE memories SET project = 'elsewhere' WHERE id = ?1",
            [&pattern],
        )
        .unwrap();
    let before = s.memory(&pattern).1;
    assert_ne!(before, "stale");
    succeeded(&s.run(&["project", "move", "old", "new"]));
    assert_eq!(
        s.memory(&pattern),
        ("elsewhere".into(), "stale".into(), s.memory(&pattern).2)
    );
}

// ─── project-id --include / --exclude ───────────────────────────────────────

/// A workspace folder holding a repository `app` with a remote, a folder
/// `plain` with no project, and a no-git folder `notes` with a key file.
fn workspace(h: &Home) -> PathBuf {
    let root = h.path("work/space");
    let app = h.repo("work/space/app");
    h.git(
        &app,
        &["remote", "add", "origin", "https://github.com/me/app"],
    );
    fs::create_dir_all(root.join("plain")).unwrap();
    fs::create_dir_all(root.join("notes")).unwrap();
    fs::write(root.join("notes").join(FILE), "team notes\n").unwrap();
    root
}

#[test]
fn include_lists_a_folder_once_and_names_its_project() {
    let h = Home::new();
    let root = workspace(&h);
    let out = h.run(&root, &["project-id", "--include", "app"]);
    succeeded(&out);
    assert!(text(&out).contains("github.com/me/app"), "{}", text(&out));
    assert_eq!(fs::read_to_string(root.join(LIST)).unwrap(), "@app\n");
    let out = h.run(&root, &["project-id", "--include", "app/"]);
    succeeded(&out);
    assert!(text(&out).contains("already listed"), "{}", text(&out));
    assert_eq!(fs::read_to_string(root.join(LIST)).unwrap(), "@app\n");
    // It makes git ignore the files, as --set does.
    assert_eq!(fs::read_to_string(h.default_ignore()).unwrap(), ADDED);
}

#[test]
fn include_refuses_what_could_not_load() {
    let h = Home::new();
    let root = workspace(&h);
    for (listed, why) in [
        ("../elsewhere", "no such folder"),
        ("missing", "no such folder"),
        ("plain", "no project"),
        ("", "no folder named"),
    ] {
        let out = h.run(&root, &["project-id", "--include", listed]);
        assert!(!out.status.success(), "{listed}: {}", text(&out));
        assert!(text(&out).contains(why), "{listed}: {}", text(&out));
        assert!(!root.join(LIST).exists(), "{listed}");
    }
}

// A folder outside the workspace is listed too, as written: the list file
// is per machine and only the folder's project key is read.
#[test]
fn include_lists_a_folder_outside_the_workspace() {
    let h = Home::new();
    let root = workspace(&h);
    let infra = h.repo("work/infra");
    h.git(
        &infra,
        &["remote", "add", "origin", "https://github.com/me/infra"],
    );
    let out = h.run(&root, &["project-id", "--include", "../infra"]);
    succeeded(&out);
    assert!(text(&out).contains("github.com/me/infra"), "{}", text(&out));
    let out = h.run(&root, &["project-id", "--include", infra.to_str().unwrap()]);
    succeeded(&out);
    assert!(text(&out).contains("already listed"), "{}", text(&out));
    assert_eq!(fs::read_to_string(root.join(LIST)).unwrap(), "@../infra\n");
}

#[test]
fn include_and_exclude_keep_comments_and_other_lines_as_written() {
    let h = Home::new();
    let root = workspace(&h);
    fs::write(
        root.join(LIST),
        "# repositories I work in\n@app   # the server\n@gone\n",
    )
    .unwrap();
    succeeded(&h.run(&root, &["project-id", "--include", "notes"]));
    assert_eq!(
        fs::read_to_string(root.join(LIST)).unwrap(),
        "# repositories I work in\n@app   # the server\n@gone\n@notes\n"
    );
    succeeded(&h.run(&root, &["project-id", "--exclude", "app"]));
    assert_eq!(
        fs::read_to_string(root.join(LIST)).unwrap(),
        "# repositories I work in\n@gone\n@notes\n"
    );
}

#[test]
fn exclude_of_a_folder_not_listed_changes_nothing() {
    let h = Home::new();
    let root = workspace(&h);
    fs::write(root.join(LIST), "@notes\n").unwrap();
    let out = h.run(&root, &["project-id", "--exclude", "app"]);
    succeeded(&out);
    assert!(text(&out).contains("not listed"), "{}", text(&out));
    assert_eq!(fs::read_to_string(root.join(LIST)).unwrap(), "@notes\n");
    let none = h.path("work/empty");
    fs::create_dir_all(&none).unwrap();
    let out = h.run(&none, &["project-id", "--exclude", "app"]);
    succeeded(&out);
    assert!(!none.join(LIST).exists());
}

#[test]
fn include_inside_a_repository_edits_the_list_at_its_root() {
    let h = Home::new();
    let root = workspace(&h);
    let repo = h.repo("work/space/mono");
    fs::create_dir_all(repo.join("src")).unwrap();
    let vendor = h.repo("work/space/mono/vendor");
    h.git(
        &vendor,
        &["remote", "add", "origin", "https://github.com/other/vendor"],
    );
    let out = h.run(&repo.join("src"), &["project-id", "--include", "vendor"]);
    succeeded(&out);
    assert_eq!(fs::read_to_string(repo.join(LIST)).unwrap(), "@vendor\n");
    assert!(!repo.join("src").join(LIST).exists());
    assert!(!root.join(LIST).exists());
}

#[test]
fn include_refuses_the_home_folder_and_a_symlinked_list() {
    let h = Home::new();
    let out = h.run(&h.home(), &["project-id", "--include", "work"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("home folder"), "{}", text(&out));
    let root = workspace(&h);
    let target = h.path("elsewhere-list");
    fs::write(&target, "").unwrap();
    symlink(&target, root.join(LIST)).unwrap();
    let out = h.run(&root, &["project-id", "--include", "app"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("symbolic link"), "{}", text(&out));
    assert_eq!(fs::read_to_string(&target).unwrap(), "");
}

#[test]
fn set_include_and_exclude_leave_a_file_they_cannot_read() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new();
    let root = workspace(&h);
    for (file, args) in [
        (FILE, &["project-id", "--set", "acme"][..]),
        (LIST, &["project-id", "--include", "app"][..]),
        (LIST, &["project-id", "--exclude", "app"][..]),
    ] {
        let path = root.join(file);
        fs::write(&path, "# kept\n@app\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
        let out = h.run(&root, args);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!out.status.success(), "{args:?}: {}", text(&out));
        assert!(
            text(&out).contains("Cannot read"),
            "{args:?}: {}",
            text(&out)
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# kept\n@app\n",
            "{args:?}"
        );
        fs::remove_file(&path).unwrap();
    }
}
