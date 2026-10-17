//! Owned process handles. Dropping a live handle reaps it off the caller's thread.
use std::process::{Child, ExitStatus};

use super::SpawnOutcome;
use crate::codex_manager::RunRecord;

pub trait RunningAgent: Send {
    fn pid(&self) -> u32;
    fn record(&self) -> Option<&RunRecord>;
    fn try_wait(&mut self) -> Result<Option<SpawnOutcome>, String>;
    fn wait(&mut self) -> Result<SpawnOutcome, String>;
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
        let process = self.process.as_mut().unwrap();
        if process.status.is_none() {
            process.child.kill().map_err(|_| "终止 Agent 进程失败")?;
        }
        Ok(())
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
