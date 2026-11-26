use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use relay_core::{Event, State};
use relay_git::verify::RuleSupervisor;
use relay_git::VerifyError;
use relay_store::TransitionInput;

use crate::service::{now_rfc3339, GroupIdentity, M1Service, OwnedGuard, ReclaimOutcome};

/// A rule may start only inside an attempt that already holds the durable
/// dispatch gate. An unregistered spawn window leaves that gate pending.
pub(crate) struct VerifyProcess<'a> {
    pub service: &'a M1Service,
    pub task_id: &'a str,
    pub attempt_no: u32,
    pub stop: &'a AtomicBool,
    before_spawn: Mutex<String>,
}

impl<'a> VerifyProcess<'a> {
    pub(crate) fn new(
        service: &'a M1Service,
        task_id: &'a str,
        attempt_no: u32,
        stop: &'a AtomicBool,
    ) -> Self {
        Self {
            service,
            task_id,
            attempt_no,
            stop,
            before_spawn: Mutex::new(String::new()),
        }
    }

    fn current(&self) -> Result<bool, VerifyError> {
        Ok(!self.stop.load(Ordering::SeqCst)
            && self
                .service
                .store
                .get_task(self.task_id)
                .map_err(|e| VerifyError::ExitUnconfirmed(e.to_string()))?
                .is_some_and(|task| {
                    task.active_attempt == self.attempt_no
                        && matches!(task.state, State::Running | State::Verifying)
                }))
    }

    fn finish_group(
        &self,
        child: &mut Child,
        identity: &GroupIdentity,
        clear: bool,
    ) -> Result<(), VerifyError> {
        let control = &self.service.group_control;
        let already_gone = matches!(child.try_wait(), Ok(Some(_)))
            && matches!(control.probe(identity.pgid), Ok(false));
        if !already_gone {
            let outcome = self.service.reclaim_group(
                identity,
                self.service.term_budget(),
                self.service.kill_budget(),
            );
            let reaped = self.service.reap_child(child);
            if outcome != ReclaimOutcome::Confirmed || !reaped {
                return Err(VerifyError::ExitUnconfirmed(format!(
                    "进程组 {}",
                    identity.pgid
                )));
            }
        }
        if clear {
            self.service
                .store
                .set_gate_pgid(self.task_id, self.attempt_no, 0, &now_rfc3339())
                .map_err(|e| VerifyError::ExitUnconfirmed(format!("无法写入回收证据：{e}")))?;
        }
        Ok(())
    }

    pub(crate) fn keep_gate_interrupted(&self, reason: &str) {
        let store = &self.service.store;
        let now = now_rfc3339();
        let _ = store.set_gate_reason(self.task_id, self.attempt_no, reason, &now);
        if let Ok(Some(task)) = store.get_task(self.task_id) {
            if task.active_attempt == self.attempt_no
                && matches!(
                    task.state,
                    State::Running | State::Verifying | State::Cancelling
                )
            {
                let _ = store.transition(TransitionInput {
                    task_id: self.task_id,
                    expected_active_attempt: self.attempt_no,
                    expected_revision: task.revision,
                    event: Event::MarkInterrupted,
                    now: &now,
                });
            }
        }
    }

    pub(crate) fn release_after_stop(&self) {
        self.service
            .confirm_workers_stopped(self.task_id, self.attempt_no);
        let _ = self.service.store.release_dispatch_gate(
            self.task_id,
            self.attempt_no,
            "验证停止且已确认进程退出",
            &now_rfc3339(),
        );
    }
}

impl RuleSupervisor for VerifyProcess<'_> {
    fn check_before_spawn(&self) -> Result<(), VerifyError> {
        if !self.current()? {
            return Err(VerifyError::Stopped);
        }
        let gates = self
            .service
            .store
            .list_pending_gates()
            .map_err(|e| VerifyError::ExitUnconfirmed(e.to_string()))?;
        if !gates
            .iter()
            .any(|g| g.task_id == self.task_id && g.attempt_no == self.attempt_no && g.pgid == 0)
        {
            return Err(VerifyError::ExitUnconfirmed(
                "验证没有可用的派发许可".into(),
            ));
        }
        *self.before_spawn.lock().expect("before_spawn lock") = relay_sys::current_epoch_token();
        Ok(())
    }

    fn wait_for_exit(
        &self,
        child: &mut Child,
        deadline: Instant,
    ) -> Result<Option<ExitStatus>, VerifyError> {
        let pgid = child.id() as i32;
        let before = self.before_spawn.lock().expect("before_spawn lock").clone();
        let leader_start = M1Service::leader_start_with_fallback(pgid, &before);
        let live_id = GroupIdentity::live(pgid, leader_start.clone());
        let after_exit_id = GroupIdentity::after_leader_exit(pgid, leader_start);
        let _gate_guard = {
            self.service
                .gate_groups
                .lock()
                .expect("gate_groups 锁中毒")
                .insert((self.task_id.to_owned(), self.attempt_no), live_id.clone());
            OwnedGuard {
                owned: Arc::clone(&self.service.gate_groups),
                key: (self.task_id.to_owned(), self.attempt_no),
            }
        };
        if let Err(e) =
            self.service
                .store
                .set_gate_pgid(self.task_id, self.attempt_no, pgid, &now_rfc3339())
        {
            self.finish_group(child, &live_id, false)?;
            return Err(VerifyError::ProcessFailed(format!(
                "无法登记验证进程组：{e}"
            )));
        }
        loop {
            match self.current() {
                Ok(true) => {}
                current => {
                    self.finish_group(child, &live_id, true)?;
                    return Err(current.err().unwrap_or(VerifyError::Stopped));
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let clean = matches!(self.service.group_control.probe(pgid), Ok(false));
                    self.finish_group(child, &after_exit_id, true)?;
                    if !clean {
                        return Err(VerifyError::ProcessFailed(
                            "规则结束时仍有后代，或无法确认当时的进程组状态".into(),
                        ));
                    }
                    return Ok(Some(status));
                }
                Ok(None) if Instant::now() >= deadline => {
                    self.finish_group(child, &live_id, true)?;
                    return Ok(None);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => {
                    self.finish_group(child, &live_id, true)?;
                    return Err(VerifyError::ProcessFailed(error.to_string()));
                }
            }
        }
    }
}
