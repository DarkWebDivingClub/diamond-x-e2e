use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};

/// Managed child process that is killed on drop.
pub struct ManagedChild {
    name: String,
    child: Option<Child>,
    log_path: PathBuf,
}

impl ManagedChild {
    /// Spawn a child process, redirecting stdout and stderr to the given log file.
    pub fn spawn(
        name: &str,
        program: &str,
        args: &[&str],
        log_dir: &Path,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_cwd(name, program, args, log_dir, None)
    }

    /// Spawn a child process with an optional working directory.
    pub fn spawn_with_cwd(
        name: &str,
        program: &str,
        args: &[&str],
        log_dir: &Path,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let log_path = log_dir.join(format!("{name}.log"));
        let log_file = std::fs::File::create(&log_path)?;
        let stderr_file = log_file.try_clone()?;

        tracing::info!("Spawning {name}: {program} {}", args.join(" "));
        tracing::info!("  log: {}", log_path.display());
        if let Some(dir) = cwd {
            tracing::info!("  cwd: {}", dir.display());
        }

        let mut cmd = Command::new(program);
        cmd.args(args)
            .envs(std::env::vars())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(stderr_file))
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let child = cmd.spawn()?;

        Ok(Self {
            name: name.to_string(),
            child: Some(child),
            log_path,
        })
    }

    /// Check that the process is still running.
    pub fn check_alive(&mut self) -> anyhow::Result<()> {
        if let Some(ref mut child) = self.child {
            match child.try_wait()? {
                Some(status) => {
                    anyhow::bail!(
                        "{} exited unexpectedly with status {}. See log: {}",
                        self.name,
                        status,
                        self.log_path.display()
                    );
                }
                None => Ok(()),
            }
        } else {
            anyhow::bail!("{} already stopped", self.name);
        }
    }

    /// Kill the child process.
    pub async fn kill(&mut self) {
        if let Some(ref mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            tracing::info!("{} killed", self.name);
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            // Best-effort synchronous kill via start_kill.
            let _ = child.start_kill();
            tracing::info!("{} killed (drop)", self.name);
        }
    }
}
