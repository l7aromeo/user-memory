//! `docker compose` for the installer's Docker server, by its compose.yaml:
//! the one way setup, the `embedding` commands and doctor run Docker for it.

use anyhow::{Context, Result, bail};
use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
};

/// The server's service in compose.yaml.
pub const SERVICE: &str = "memory";

pub struct Compose {
    docker: PathBuf,
    file: PathBuf,
}

impl Compose {
    /// `docker` on PATH, for the project whose compose.yaml is `file`.
    pub fn new(file: PathBuf) -> Self {
        Self::with_docker(PathBuf::from("docker"), file)
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
