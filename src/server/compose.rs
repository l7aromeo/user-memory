//! `docker compose` for the installer's Docker server, by its compose.yaml:
//! the one way setup, the `embedding` commands and doctor run Docker for it.

use anyhow::{Context, Result, bail};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, ExitStatus, Output, Stdio},
    thread,
};

/// The server's service in compose.yaml.
pub const SERVICE: &str = "memory";

/// A Docker command that ran with its output shown: how it ended and what
/// it printed, stdout then stderr.
pub struct Shown {
    pub status: ExitStatus,
    pub said: String,
}

/// Copies `from` to `to` as it comes, and returns what it copied.
fn kept(mut from: impl Read, to: impl Write) -> String {
    let mut copy = Tee {
        to,
        kept: Vec::new(),
    };
    let _ = io::copy(&mut from, &mut copy);
    String::from_utf8_lossy(&copy.kept).into_owned()
}

/// A writer that passes what it is given to `to` and keeps a copy. A
/// failed write to `to`, such as a closed terminal, still keeps it.
struct Tee<W> {
    to: W,
    kept: Vec<u8>,
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.kept.extend_from_slice(bytes);
        let _ = self.to.write_all(bytes).and_then(|()| self.to.flush());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let _ = self.to.flush();
        Ok(())
    }
}

/// The Docker command the installers and the user's shell run: the first
/// `docker` on PATH, or the bare name, which then fails as not found.
pub fn docker() -> PathBuf {
    crate::setup::on_path("docker").unwrap_or_else(|| PathBuf::from("docker"))
}

pub struct Compose {
    docker: PathBuf,
    file: PathBuf,
}

impl Compose {
    /// [`docker`]'s Docker, for the project whose compose.yaml is `file`.
    pub fn new(file: PathBuf) -> Self {
        Self::with_docker(docker(), file)
    }

    /// [`Compose::new`] with `docker` as the Docker command.
    pub fn with_docker(docker: PathBuf, file: PathBuf) -> Self {
        Self { docker, file }
    }

    /// `docker compose -f FILE ARGS`, for the caller to run.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.docker);
        command.arg("compose").arg("-f").arg(&self.file).args(args);
        command
    }

    /// Whether `args` succeeded, with `input` as stdin and Docker's output
    /// shown as it comes.
    pub fn status(&self, args: &[&str], input: Stdio) -> Result<bool> {
        Ok(self
            .command(args)
            .stdin(input)
            .status()
            .context("Cannot run docker")?
            .success())
    }

    /// Runs `args` with Docker's output shown; fails naming the command.
    pub fn run(&self, args: &[&str]) -> Result<()> {
        if !self.status(args, Stdio::inherit())? {
            bail!("docker compose {} failed (above)", args.join(" "));
        }
        Ok(())
    }

    /// Runs `args` with `input` as stdin, showing Docker's output as it
    /// comes and keeping it: how the command ended and what it printed.
    pub fn shown(&self, args: &[&str], input: Stdio) -> Result<Shown> {
        let mut child = self
            .command(args)
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Cannot run docker")?;
        let out = child.stdout.take().expect("stdout is piped");
        let err = child.stderr.take().expect("stderr is piped");
        let out = thread::spawn(move || kept(out, io::stdout()));
        let err = thread::spawn(move || kept(err, io::stderr()));
        let status = child.wait().context("Cannot run docker")?;
        let mut said = out.join().expect("showing Docker's output does not panic");
        said += &err.join().expect("showing Docker's output does not panic");
        Ok(Shown { status, said })
    }

    /// Runs `args` with `input` as stdin and Docker's errors shown; returns
    /// its trimmed output, or fails naming the command.
    pub fn output(&self, args: &[&str], input: Stdio) -> Result<String> {
        let out = self
            .command(args)
            .stdin(input)
            .stderr(Stdio::inherit())
            .output()
            .context("Cannot run docker")?;
        if !out.status.success() {
            bail!("docker compose {} failed (above)", args.join(" "));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// `user-memory ARGS` in the running server's container, with `env`
    /// set there and no stdin: its whole output, for the caller to show.
    pub fn exec(&self, env: &[(&str, &str)], args: &[&str]) -> Result<Output> {
        let mut command = self.command(&["exec", "-T"]);
        for (name, value) in env {
            command.arg("-e").arg(format!("{name}={value}"));
        }
        command
            .args([SERVICE, "user-memory"])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("Cannot run docker")
    }

    /// The command a user runs for `user-memory ARGS` in the container.
    pub fn exec_hint(&self, args: &str) -> String {
        format!(
            "docker compose -f {} exec {SERVICE} user-memory {args}",
            crate::shell::quote(&self.file)
        )
    }

    /// Runs `script` in a one-off server container's shell, with the
    /// volume, returning its output.
    pub fn shell(&self, script: &str, input: Stdio) -> Result<String> {
        self.output(
            &[
                "run",
                "--rm",
                "-T",
                "--entrypoint",
                "sh",
                SERVICE,
                "-c",
                script,
            ],
            input,
        )
    }
}
