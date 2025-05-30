// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

// #[PerformanceCriticalPath]
use txn_types::{Key, TimeStamp};

use crate::storage::{
    ProcessResult, Snapshot,
    kv::WriteData,
    lock_manager::LockManager,
    mvcc::{MvccTxn, SnapshotReader},
    txn::{
        Result, cleanup,
        commands::{
            Command, CommandExt, ReaderWithStats, ReleasedLocks, ResponsePolicy, TypedCommand,
            WriteCommand, WriteContext, WriteResult,
        },
        commit,
    },
};

command! {
    /// Resolve locks on `resolve_keys` according to `start_ts` and `commit_ts`.
    ResolveLockLite:
        cmd_ty => (),
        display => {
            "kv::resolve_lock_lite resolve_keys({:?}) {} {} | {:?}",
            (resolve_keys, start_ts, commit_ts, ctx),
        }
        content => {
            /// The transaction timestamp.
            start_ts: TimeStamp,
            /// The transaction commit timestamp.
            commit_ts: TimeStamp,
            /// The keys to resolve.
            resolve_keys: Vec<Key>,
        }
        in_heap => {
            resolve_keys,
        }
}

impl CommandExt for ResolveLockLite {
    ctx!();
    tag!(resolve_lock_lite);
    request_type!(KvResolveLock);
    ts!(start_ts);
    property!(is_sys_cmd);
    write_bytes!(resolve_keys: multiple);
    gen_lock!(resolve_keys: multiple);
}

impl<S: Snapshot, L: LockManager> WriteCommand<S, L> for ResolveLockLite {
    fn process_write(self, snapshot: S, context: WriteContext<'_, L>) -> Result<WriteResult> {
        let mut txn = MvccTxn::new(self.start_ts, context.concurrency_manager);
        let mut reader = ReaderWithStats::new(
            SnapshotReader::new_with_ctx(self.start_ts, snapshot, &self.ctx),
            context.statistics,
        );

        let rows = self.resolve_keys.len();
        // ti-client guarantees the size of resolve_keys will not too large, so no
        // necessary to control the write_size as ResolveLock.
        let mut released_locks = ReleasedLocks::new();
        for key in self.resolve_keys {
            released_locks.push(if !self.commit_ts.is_zero() {
                commit(&mut txn, &mut reader, key, self.commit_ts, None)?
            } else {
                cleanup(&mut txn, &mut reader, key, TimeStamp::zero(), false)?
            });
        }

        let known_txn_status = if !self.commit_ts.is_zero() {
            vec![(self.start_ts, self.commit_ts)]
        } else {
            vec![]
        };
        let new_acquired_locks = txn.take_new_locks();
        let mut write_data = WriteData::from_modifies(txn.into_modifies());
        write_data.set_allowed_on_disk_almost_full();
        Ok(WriteResult {
            ctx: self.ctx,
            to_be_write: write_data,
            rows,
            pr: ProcessResult::Res,
            lock_info: vec![],
            released_locks,
            new_acquired_locks,
            lock_guards: vec![],
            response_policy: ResponsePolicy::OnApplied,
            known_txn_status,
        })
    }
}
