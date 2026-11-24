use super::*;

pub(super) const STAGE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS stage_runs (
    task_id TEXT NOT NULL,
    attempt_no INTEGER NOT NULL,
    round INTEGER NOT NULL CHECK (round > 0),
    stage TEXT NOT NULL CHECK (stage IN ('implement','verify','review')),
    state TEXT NOT NULL CHECK (state IN ('active','succeeded','needs_changes','failed','interrupted','cancelled')),
    started_at TEXT NOT NULL,
    ended_at TEXT,
    diff_sha256 TEXT,
    receipt_sha256 TEXT,
    PRIMARY KEY (task_id, attempt_no, round, stage),
    FOREIGN KEY (task_id, attempt_no) REFERENCES attempts(task_id, attempt_no),
    CHECK ((diff_sha256 IS NULL) = (receipt_sha256 IS NULL))
);
CREATE UNIQUE INDEX IF NOT EXISTS stage_runs_one_active ON stage_runs(task_id, attempt_no)
    WHERE state = 'active';
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKind {
    Implement,
    Verify,
    Review,
}

impl StageKind {
    fn name(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::Verify => "verify",
            Self::Review => "review",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StageKey<'a> {
    pub task_id: &'a str,
    pub attempt_no: u32,
    pub round: u32,
    pub kind: StageKind,
}

/// Read-only StageRun projection for the task window and diagnostic views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRunRecord {
    pub task_id: String,
    pub attempt_no: u32,
    pub round: u32,
    pub stage: String,
    pub state: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub diff_sha256: Option<String>,
    pub receipt_sha256: Option<String>,
}

impl Store {
    /// List persisted stages for one attempt in round/order order.
    pub fn list_stage_runs(
        &self,
        task_id: &str,
        attempt_no: u32,
    ) -> Result<Vec<StageRunRecord>, StoreError> {
        validate_id(task_id, "task_id")?;
        if attempt_no == 0 {
            return Err(StoreError::InvalidInput("attempt_no 必须为正整数".into()));
        }
        let rows = self
            .lock_conn()?
            .prepare(
                "SELECT task_id, attempt_no, round, stage, state, started_at, ended_at,
                        diff_sha256, receipt_sha256
                 FROM stage_runs WHERE task_id = ?1 AND attempt_no = ?2
                 ORDER BY round ASC, CASE stage
                    WHEN 'implement' THEN 1 WHEN 'verify' THEN 2 WHEN 'review' THEN 3 ELSE 4 END",
            )?
            .query_map(params![task_id, i64::from(attempt_no)], |r| {
                Ok(StageRunRecord {
                    task_id: r.get(0)?,
                    attempt_no: r.get::<_, i64>(1)? as u32,
                    round: r.get::<_, i64>(2)? as u32,
                    stage: r.get(3)?,
                    state: r.get(4)?,
                    started_at: r.get(5)?,
                    ended_at: r.get(6)?,
                    diff_sha256: r.get(7)?,
                    receipt_sha256: r.get(8)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum StageFinish<'a> {
    Implemented,
    Verified { diff: &'a str, receipt: &'a str },
    Reviewed { needs_changes: bool },
}

impl Store {
    pub fn has_active_stage(&self, task_id: &str, attempt_no: u32) -> Result<bool, StoreError> {
        Ok(self.lock_conn()?.query_row(
            "SELECT EXISTS (SELECT 1 FROM stage_runs WHERE task_id=?1 AND attempt_no=?2 AND state='active')",
            params![task_id, attempt_no], |r| r.get(0),
        )?)
    }

    /// Persist a one-use start permit before performing any stage work. The unique
    /// execution key is immutable and survives a crash; it is never reactivated.
    pub fn begin_stage(
        &self,
        key: StageKey<'_>,
        expected_revision: u64,
        candidate: Option<(&str, &str)>,
        now: &str,
    ) -> Result<(), StoreError> {
        validate_id(key.task_id, "task_id")?;
        require_time(now, "now")?;
        let expected_revision = i64::try_from(expected_revision)
            .map_err(|_| StoreError::InvalidInput("阶段 revision 超出范围".into()))?;
        if key.round == 0 || (key.kind == StageKind::Review) != candidate.is_some() {
            return Err(StoreError::InvalidInput("阶段轮次或摘要参数无效".into()));
        }
        let parent = if key.round == 1 && key.kind == StageKind::Implement {
            "launching"
        } else {
            "running"
        };
        let (diff, receipt) = candidate.unzip();
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO stage_runs
                (task_id, attempt_no, round, stage, state, started_at, diff_sha256, receipt_sha256)
             SELECT t.task_id, t.active_attempt, ?3, ?4, 'active', ?7, ?8, ?9
             FROM tasks t
             JOIN attempts a ON a.task_id=t.task_id AND a.attempt_no=t.active_attempt
             JOIN dispatch_gate g ON g.task_id=t.task_id AND g.attempt_no=t.active_attempt
             WHERE t.task_id=?1 AND t.active_attempt=?2 AND t.state=?5 AND t.revision=?6
               AND a.state=t.state AND a.revision=t.revision AND g.state='pending' AND g.pgid=0
               AND NOT EXISTS (
                   SELECT 1 FROM stage_runs s WHERE s.task_id=?1 AND s.attempt_no=?2
                     AND (s.state='active' OR (s.round=?3 AND s.stage=?4)))
               AND ((?4='implement' AND ?3=1) OR EXISTS (
                   SELECT 1 FROM stage_runs p WHERE p.task_id=?1 AND p.attempt_no=?2 AND (
                     (?4='implement' AND p.round=?3-1 AND p.stage='review' AND p.state='needs_changes')
                     OR (?4='verify' AND p.round=?3 AND p.stage='implement' AND p.state='succeeded')
                     OR (?4='review' AND p.round=?3 AND p.stage='verify' AND p.state='succeeded'
                         AND p.diff_sha256=?8 AND p.receipt_sha256=?9))))",
            params![key.task_id, key.attempt_no, key.round, key.kind.name(), parent,
                expected_revision, now, diff, receipt],
        )?;
        if inserted != 1 {
            return Err(StoreError::InvalidInput(
                "阶段不可启动：已获许可、父任务已变更或前序未通过".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Only the owner of a still-current running attempt can commit its stage
    /// result. Review digests are frozen at start; completion cannot replace them.
    pub fn finish_stage(
        &self,
        key: StageKey<'_>,
        expected_revision: u64,
        result: StageFinish<'_>,
        now: &str,
    ) -> Result<(), StoreError> {
        require_time(now, "now")?;
        let expected_revision = i64::try_from(expected_revision)
            .map_err(|_| StoreError::InvalidInput("阶段 revision 超出范围".into()))?;
        fn digest(s: &str) -> bool {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        let (state, diff, receipt) = match (key.kind, result) {
            (StageKind::Implement, StageFinish::Implemented) => ("succeeded", None, None),
            (StageKind::Verify, StageFinish::Verified { diff, receipt })
                if digest(diff) && digest(receipt) =>
            {
                ("succeeded", Some(diff), Some(receipt))
            }
            (StageKind::Review, StageFinish::Reviewed { needs_changes }) => (
                if needs_changes {
                    "needs_changes"
                } else {
                    "succeeded"
                },
                None,
                None,
            ),
            _ => return Err(StoreError::InvalidInput("阶段结论或摘要无效".into())),
        };
        let updated = self.lock_conn()?.execute(
            "UPDATE stage_runs SET state=?6, ended_at=?7,
                diff_sha256=coalesce(?8,diff_sha256), receipt_sha256=coalesce(?9,receipt_sha256)
             WHERE task_id=?1 AND attempt_no=?2 AND round=?3 AND stage=?4 AND state='active'
               AND EXISTS (
                 SELECT 1 FROM tasks t JOIN attempts a
                   ON a.task_id=t.task_id AND a.attempt_no=t.active_attempt
                 WHERE t.task_id=?1 AND t.active_attempt=?2 AND t.revision=?5
                   AND t.state='running' AND a.state=t.state AND a.revision=t.revision)",
            params![
                key.task_id,
                key.attempt_no,
                key.round,
                key.kind.name(),
                expected_revision,
                state,
                now,
                diff,
                receipt
            ],
        )?;
        if updated != 1 {
            return Err(StoreError::InvalidInput("阶段已结束或父任务已变更".into()));
        }
        Ok(())
    }
}

pub(super) fn close_active_stage(
    tx: &Transaction<'_>,
    task_id: &str,
    attempt_no: i64,
    next: &str,
    now: &str,
) -> Result<(), StoreError> {
    if matches!(next, "failed" | "interrupted" | "cancelled") {
        let affected = tx.execute(
            "UPDATE stage_runs SET state=?3, ended_at=?4
             WHERE task_id=?1 AND attempt_no=?2 AND state='active'",
            params![task_id, attempt_no, next, now],
        )?;
        // A silently ignored update may not leave a live stage on a terminal task.
        if affected == 0
            && tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM stage_runs
             WHERE task_id=?1 AND attempt_no=?2 AND state='active')",
                params![task_id, attempt_no],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Err(StoreError::Environment("阶段未能随父任务收口".into()));
        }
    }
    Ok(())
}

pub(super) fn require_reviewed_candidate(
    tx: &Transaction<'_>,
    task_id: &str,
    attempt_no: u32,
    diff: &str,
    receipt: &str,
) -> Result<(), StoreError> {
    let accepted = tx.query_row(
        "SELECT CASE WHEN NOT EXISTS (
           SELECT 1 FROM stage_runs WHERE task_id=?1 AND attempt_no=?2
         ) THEN 1 ELSE EXISTS (
           SELECT 1 FROM stage_runs r
           WHERE r.task_id=?1 AND r.attempt_no=?2 AND r.stage='review' AND r.state='succeeded'
             AND r.diff_sha256=?3 AND r.receipt_sha256=?4
             AND r.round=(SELECT MAX(round) FROM stage_runs WHERE task_id=?1 AND attempt_no=?2)
         ) END",
        params![task_id, attempt_no, diff, receipt],
        |r| r.get::<_, bool>(0),
    )?;
    if !accepted {
        return Err(StoreError::InvalidInput(
            "候选与最新通过评审的阶段摘要不一致".into(),
        ));
    }
    Ok(())
}

/// Offline v8 → v9. Call with the instance stopped and its exclusive lock held.
pub fn migrate_v8_to_v9(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if !matches!(std::fs::symlink_metadata(backup_path), Err(ref e) if e.kind()==std::io::ErrorKind::NotFound)
    {
        return Err(StoreError::InvalidInput("备份路径已存在或无法检查".into()));
    }
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    if read_schema_version(&conn)? != Some(8) {
        return Err(StoreError::InvalidInput(
            "仅支持从 schema v8 迁移到 v9".into(),
        ));
    }
    let backup = backup_path
        .to_str()
        .ok_or_else(|| StoreError::InvalidInput("备份路径不是 UTF-8".into()))?;
    conn.execute("VACUUM INTO ?1", params![backup])?;
    let probe = Connection::open_with_flags(backup_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if read_schema_version(&probe)? != Some(8)
        || probe.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))? != "ok"
    {
        return Err(StoreError::Environment("v8 备份检查失败".into()));
    }
    drop(probe);
    std::fs::File::open(backup_path)
        .and_then(|file| file.sync_all())
        .and_then(|_| {
            std::fs::File::open(
                backup_path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()
        })
        .map_err(|e| StoreError::Environment(format!("备份同步失败：{e}")))?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(STAGE_SCHEMA)?;
    let updated = tx.execute(
        "UPDATE meta SET value='9' WHERE key='schema_version' AND value='8'",
        [],
    )?;
    if updated != 1 || read_schema_version(&tx)? != Some(9) {
        return Err(StoreError::Environment(
            "v9 版本写入失败，迁移已回滚".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}
