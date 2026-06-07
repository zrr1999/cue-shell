use cue_core::ScriptId;
use cue_core::ipc::{ScriptItemInfo, ScriptItemResult, ScriptSubmitError};
use cue_core::mode::Mode;

use crate::storage;

#[derive(Debug, Clone, Copy)]
pub(super) enum ScriptFinish {
    Done {
        exit_code: i32,
    },
    Failed {
        exit_code: i32,
        failed_item_index: Option<usize>,
    },
}

impl ScriptFinish {
    pub(super) fn done(exit_code: i32) -> Self {
        Self::Done { exit_code }
    }

    pub(super) fn failed(exit_code: i32, failed_item_index: Option<usize>) -> Self {
        Self::Failed {
            exit_code,
            failed_item_index,
        }
    }

    fn stored_status(self) -> storage::StoredScriptRunStatus {
        match self {
            Self::Done { .. } => storage::StoredScriptRunStatus::Done,
            Self::Failed { .. } => storage::StoredScriptRunStatus::Failed,
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Self::Done { exit_code } | Self::Failed { exit_code, .. } => exit_code,
        }
    }

    fn failed_item_index(self) -> Option<usize> {
        match self {
            Self::Done { .. } => None,
            Self::Failed {
                failed_item_index, ..
            } => failed_item_index,
        }
    }
}

struct ScriptRecordUpdate<'a> {
    script_id: ScriptId,
    mode: Mode,
    items: &'a [ScriptItemInfo],
    status: storage::StoredScriptRunStatus,
    submit_error: Option<&'a ScriptSubmitError>,
    exit_code: Option<i32>,
    failed_item_index: Option<usize>,
}

pub(super) async fn persist_submission(
    script_id: ScriptId,
    mode: Mode,
    items: &[ScriptItemInfo],
    submit_error: Option<&ScriptSubmitError>,
    db: &storage::SharedConnection,
) -> anyhow::Result<()> {
    let status = if submit_error.is_some() {
        storage::StoredScriptRunStatus::PartialError
    } else {
        storage::StoredScriptRunStatus::Submitted
    };
    persist_record(
        ScriptRecordUpdate {
            script_id,
            mode,
            items,
            status,
            submit_error,
            exit_code: None,
            failed_item_index: None,
        },
        db,
    )
    .await
    .map_err(|error| anyhow::anyhow!("persist script {script_id} submission: {error}"))
}

pub(super) async fn persist_finished(
    script_id: ScriptId,
    mode: Mode,
    items: &[ScriptItemInfo],
    finish: ScriptFinish,
    submit_error: Option<&ScriptSubmitError>,
    db: &storage::SharedConnection,
) -> anyhow::Result<()> {
    persist_record(
        ScriptRecordUpdate {
            script_id,
            mode,
            items,
            status: finish.stored_status(),
            submit_error,
            exit_code: Some(finish.exit_code()),
            failed_item_index: finish.failed_item_index(),
        },
        db,
    )
    .await
    .map_err(|error| anyhow::anyhow!("persist script {script_id} completion: {error}"))
}

async fn persist_record(
    update: ScriptRecordUpdate<'_>,
    db: &storage::SharedConnection,
) -> anyhow::Result<()> {
    let run = stored_script_run(&update);
    let stored_items = update
        .items
        .iter()
        .map(|item| stored_script_item(update.script_id, item))
        .collect::<Vec<_>>();
    storage::with_connection(db, move |conn| {
        storage::upsert_script_run(conn, &run, &stored_items)
    })
    .await
}

fn stored_script_run(update: &ScriptRecordUpdate<'_>) -> storage::StoredScriptRun {
    storage::StoredScriptRun {
        id: update.script_id.to_string(),
        mode: match update.mode {
            Mode::Job => "job".into(),
            Mode::Cron => "cron".into(),
        },
        input: update
            .items
            .iter()
            .map(|item| item.source.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        status: update.status,
        item_count: update.items.len(),
        error_code: update.submit_error.map(|error| error.code.clone()),
        error_message: update.submit_error.map(|error| error.message.clone()),
        exit_code: update.exit_code,
        failed_item_index: update.failed_item_index,
    }
}

fn stored_script_item(script_id: ScriptId, item: &ScriptItemInfo) -> storage::StoredScriptItem {
    storage::StoredScriptItem {
        script_id: script_id.to_string(),
        item_index: item.index,
        source_text: item.source.clone(),
        kind: match &item.result {
            ScriptItemResult::Job { .. } => "job".into(),
            ScriptItemResult::Chain { .. } => "chain".into(),
            ScriptItemResult::Cron { .. } => "cron".into(),
            ScriptItemResult::Message { .. } => "message".into(),
        },
        target_id: match &item.result {
            ScriptItemResult::Job { job_id, .. } => Some(job_id.clone()),
            ScriptItemResult::Chain { chain_id, .. } => Some(chain_id.clone()),
            ScriptItemResult::Cron { cron_id } => Some(cron_id.clone()),
            ScriptItemResult::Message { .. } => None,
        },
        chain_id: match &item.result {
            ScriptItemResult::Chain { chain_id, .. } => Some(chain_id.clone()),
            _ => None,
        },
        job_ids: match &item.result {
            ScriptItemResult::Job { job_id, .. } => vec![job_id.clone()],
            ScriptItemResult::Chain { job_ids, .. } => job_ids.clone(),
            _ => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cue_core::ipc::{ChainInfo, ScriptItemInfo, ScriptItemResult};

    #[test]
    fn stored_script_item_preserves_chain_targets_and_jobs() {
        let item = ScriptItemInfo {
            index: 2,
            source: "echo ok && echo next".into(),
            result: ScriptItemResult::Chain {
                chain_id: "CH7".into(),
                job_ids: vec!["J4".into(), "J5".into()],
                chain: ChainInfo {
                    id: "CH7".into(),
                    pipeline: "echo ok && echo next".into(),
                    total_jobs: 2,
                    jobs: vec![],
                },
            },
        };

        let stored = stored_script_item(ScriptId(3), &item);

        assert_eq!(stored.script_id, "R3");
        assert_eq!(stored.item_index, 2);
        assert_eq!(stored.source_text, "echo ok && echo next");
        assert_eq!(stored.kind, "chain");
        assert_eq!(stored.target_id.as_deref(), Some("CH7"));
        assert_eq!(stored.chain_id.as_deref(), Some("CH7"));
        assert_eq!(stored.job_ids, vec!["J4", "J5"]);
    }
}
