//! Subprocess launching: log capture, readiness by log line, kill on drop.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// How much of each log is quoted in an error.
const LOG_TAIL: usize = 4000;

/// A spawned child whose stdout and stderr are copied to files.
///
/// Dropping it kills the child. [`Process::stop`] shuts it down gracefully.
#[derive(Debug)]
pub struct Process {
    name: &'static str,
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

/// The binary for `name`: `$env_var` when set, else `name` on `PATH`.
pub fn binary(name: &str, env_var: &str) -> PathBuf {
    std::env::var_os(env_var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(name))
}

impl Process {
    /// Spawns `program` with `args`, logging to `<log_dir>/<name>.{stdout,stderr}.log`.
    pub fn spawn<I, S>(
        name: &'static str,
        program: &Path,
        args: I,
        log_dir: &Path,
        stdin: Stdio,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let stdout = log_dir.join(format!("{name}.stdout.log"));
        let stderr = log_dir.join(format!("{name}.stderr.log"));
        let child = Command::new(program)
            .args(args)
            .stdin(stdin)
            .stdout(std::fs::File::create(&stdout)?)
            .stderr(std::fs::File::create(&stderr)?)
            .spawn()
            .with_context(|| format!("spawn {name} ({})", program.display()))?;
        Ok(Self {
            name,
            child,
            stdout,
            stderr,
        })
    }

    /// The stdout log so far.
    pub fn stdout(&self) -> String {
        std::fs::read_to_string(&self.stdout).unwrap_or_default()
    }

    /// The stderr log so far.
    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// Tails of both logs, for error messages.
    pub fn log_tail(&self) -> String {
        format!(
            "--- {name} stdout ---\n{}\n--- {name} stderr ---\n{}",
            tail(&self.stdout()),
            tail(&self.stderr()),
            name = self.name
        )
    }

    /// `Some(status)` once the child has exited.
    pub fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Polls the logs every 100 ms until a line containing `needle` appears
    /// and returns that whole line, or fails on exit, `timeout`, or a line
    /// containing one of `fatal`.
    pub async fn wait_for_line(
        &mut self,
        needle: &str,
        fatal: &[&str],
        timeout: Duration,
    ) -> Result<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.exited()? {
                bail!(
                    "{} exited ({status}) before logging {needle:?}\n{}",
                    self.name,
                    self.log_tail()
                );
            }
            let logs = format!("{}\n{}", self.stdout(), self.stderr());
            if let Some(line) = logs.lines().find(|line| line.contains(needle)) {
                return Ok(line.to_string());
            }
            if let Some(line) = logs
                .lines()
                .find(|line| fatal.iter().any(|f| line.contains(f)))
            {
                bail!(
                    "{} logged a fatal line: {line}\n{}",
                    self.name,
                    self.log_tail()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "{} did not log {needle:?} within {timeout:?}\n{}",
                    self.name,
                    self.log_tail()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// SIGINT, then waits up to `grace` for a clean exit, then SIGKILL.
    pub async fn stop(&mut self, grace: Duration) -> Result<()> {
        if self.exited()?.is_some() {
            return Ok(());
        }
        let pid = Pid::from_raw(i32::try_from(self.child.id())?);
        kill(pid, Signal::SIGINT).with_context(|| format!("SIGINT {}", self.name))?;
        let deadline = tokio::time::Instant::now() + grace;
        while self.exited()?.is_none() {
            if tokio::time::Instant::now() >= deadline {
                self.child.kill()?;
                self.child.wait()?;
                bail!("{} ignored SIGINT for {grace:?}; killed", self.name);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn tail(log: &str) -> &str {
    let start = log.len().saturating_sub(LOG_TAIL);
    let start = log
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= start)
        .unwrap_or(log.len());
    &log[start..]
}

/// Runs `program` with `args` to completion, feeding `stdin`, and returns its
/// stdout. A non-zero exit fails with both streams quoted.
pub async fn run(program: &Path, args: &[String], stdin: Option<&str>) -> Result<String> {
    let program = program.to_path_buf();
    let args = args.to_vec();
    let stdin = stdin.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut child = Command::new(&program)
            .args(&args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {}", program.display()))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input.as_bytes())?;
            pipe.write_all(b"\n")?;
        }
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            bail!(
                "{} {} failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
                program.display(),
                args.join(" "),
                output.status,
                tail(&stdout),
                tail(&String::from_utf8_lossy(&output.stderr))
            );
        }
        Ok(stdout)
    })
    .await?
}

/// Polls `probe` every `interval` until it returns `Some`, or fails after `timeout`.
pub async fn poll<T, F, Fut>(
    what: &str,
    interval: Duration,
    timeout: Duration,
    mut probe: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = probe().await {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("{what} did not happen within {timeout:?}");
        }
        tokio::time::sleep(interval).await;
    }
}

/// Copies `src` into `dst` recursively (`dst` is created).
///
/// Through `cp --sparse=always`, since an LMDB map is a sparse file that a
/// plain copy would expand to its full size.
pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    let status = Command::new("cp")
        .arg("-a")
        .arg("--sparse=always")
        .arg(src.join("."))
        .arg(dst)
        .status()
        .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    if !status.success() {
        bail!(
            "cp {} to {} failed ({status})",
            src.display(),
            dst.display()
        );
    }
    Ok(())
}
