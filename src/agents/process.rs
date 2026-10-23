//! Owned process handles. Dropping a live handle reaps it off the caller's thread.
use std::process::{Child, ExitStatus};

use super::SpawnOutcome;
use crate::runs::RunRecord;

pub trait RunningAgent: Send {
    fn pid(&self) -> u32;
    fn record(&self) -> Option<&RunRecord>;
    fn try_wait(&mut self) -> Result<Option<SpawnOutcome>, String>;
    fn wait(&mut self) -> Result<SpawnOutcome, String>;
    /// Ask the owned child to stop, with a bounded grace period before forcing it.
    /// Wrappers can forward the signal; arbitrary descendant trees are not owned.
    fn terminate(&mut self) -> Result<(), String>;
}

pub(crate) type Finish = Box<dyn FnMut(ExitStatus) -> Result<RunRecord, String> + Send>;

struct Process {
    child: Child,
    record: Option<RunRecord>,
    status: Option<ExitStatus>,
    finished: bool,
    finish: Option<Finish>,
}

impl Process {
    fn stop(&mut self) -> Result<(), String> {
        if self.status.is_some() {
            return Ok(());
        }
        self.status = self
            .child
            .try_wait()
            .map_err(|_| "查询 Agent 退出状态失败")?;
        if self.status.is_some() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            // The owned, unreaped child prevents PID reuse. Do not signal its
            // process group: interactive agents may share Usage's foreground TTY
            // group. SIGTERM lets launch wrappers forward shutdown to the agent.
            let sent = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            if sent != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err("终止 Agent 进程失败".into());
            }
            let until = std::time::Instant::now() + std::time::Duration::from_millis(500);
            loop {
                self.status = self
                    .child
                    .try_wait()
                    .map_err(|_| "查询 Agent 退出状态失败")?;
                if self.status.is_some() {
                    return Ok(());
                }
                if std::time::Instant::now() >= until {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        self.child.kill().map_err(|_| "终止 Agent 进程失败".into())
    }

    fn complete(&mut self, status: ExitStatus) -> Result<SpawnOutcome, String> {
        self.status = Some(status);
        if !self.finished {
            if let Some(finish) = &mut self.finish {
                self.record = Some(finish(status)?);
            }
            self.finish = None;
            self.finished = true;
        }
        Ok(SpawnOutcome {
            status,
            run: self.record.clone(),
        })
    }
    fn wait(&mut self) -> Result<SpawnOutcome, String> {
        let status = match self.status {
            Some(status) => status,
            None => self.child.wait().map_err(|_| "等待 Agent 进程失败")?,
        };
        self.complete(status)
    }
}

pub struct ProcessHandle {
    process: Option<Process>,
}
impl ProcessHandle {
    pub(crate) fn new(child: Child, record: Option<RunRecord>, finish: Option<Finish>) -> Self {
        Self {
            process: Some(Process {
                child,
                record,
                status: None,
                finished: false,
                finish,
            }),
        }
    }
}
impl RunningAgent for ProcessHandle {
    fn pid(&self) -> u32 {
        self.process.as_ref().unwrap().child.id()
    }
    fn record(&self) -> Option<&RunRecord> {
        self.process.as_ref().unwrap().record.as_ref()
    }
    fn try_wait(&mut self) -> Result<Option<SpawnOutcome>, String> {
        let process = self.process.as_mut().unwrap();
        let status = match process.status {
            Some(status) => Some(status),
            None => process
                .child
                .try_wait()
                .map_err(|_| "查询 Agent 退出状态失败")?,
        };
        status.map(|status| process.complete(status)).transpose()
    }
    fn wait(&mut self) -> Result<SpawnOutcome, String> {
        self.process.as_mut().unwrap().wait()
    }
    fn terminate(&mut self) -> Result<(), String> {
        self.process.as_mut().unwrap().stop()
    }
}
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take().filter(|process| !process.finished)
            && std::thread::Builder::new()
                .name("usage-agent-wait".into())
                .spawn(move || {
                    if process.wait().is_err() {
                        tracing::warn!(event = "agent_reap_failed");
                    }
                })
                .is_err()
        {
            tracing::warn!(event = "agent_reap_unavailable");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("usage-terminate-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn wait_ready(&self) {
            let until = Instant::now() + Duration::from_secs(5);
            while !self.0.join("ready").exists() {
                assert!(
                    Instant::now() < until,
                    "synthetic child did not become ready"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // The fixtures have a finite lifetime even if an assertion fails.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn terminate_allows_wrapper_to_forward_and_reap_its_agent() {
        let fixture = Fixture::new();
        let child = Command::new("python3")
            .args([
                "-c",
                r#"
import pathlib, signal, subprocess, sys
root = pathlib.Path(sys.argv[1])
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(10)'])
def stop(signum, frame):
    child.terminate()
signal.signal(signal.SIGTERM, stop)
(root / 'child.pid').write_text(str(child.pid))
(root / 'ready').touch()
status = child.wait()
(root / 'reaped').write_text(str(status))
sys.exit(7)
"#,
            ])
            .arg(&fixture.0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut handle = ProcessHandle::new(child, None, None);
        fixture.wait_ready();
        let descendant: i32 = std::fs::read_to_string(fixture.0.join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        handle.terminate().unwrap();
        assert_eq!(handle.wait().unwrap().status.code(), Some(7));
        assert!(fixture.0.join("reaped").is_file());
        // SAFETY: signal 0 only checks the synthetic process's existence.
        assert_eq!(unsafe { libc::kill(descendant, 0) }, -1);
        handle.terminate().unwrap();
    }

    #[test]
    fn terminate_forces_uncooperative_owned_child_after_grace_period() {
        let fixture = Fixture::new();
        let child = Command::new("python3")
            .args(["-c", "import pathlib,signal,sys,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); pathlib.Path(sys.argv[1], 'ready').touch(); time.sleep(10)"])
            .arg(&fixture.0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut handle = ProcessHandle::new(child, None, None);
        fixture.wait_ready();
        let started = Instant::now();
        handle.terminate().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(handle.wait().unwrap().status.signal(), Some(libc::SIGKILL));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
