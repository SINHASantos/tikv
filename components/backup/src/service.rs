// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::{Arc, Mutex, atomic::*};

use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt, channel::mpsc};
use futures_util::stream::AbortHandle;
use grpcio::{self, *};
use kvproto::brpb::*;
use raftstore::store::snapshot_backup::SnapshotBrHandle;
use tikv_util::{error, info, warn, worker::*};

use super::Task;
use crate::disk_snap::{self, StreamHandleLoop};

/// Service handles the RPC messages for the `Backup` service.
#[derive(Clone)]
pub struct Service<H: SnapshotBrHandle> {
    scheduler: Scheduler<Task>,
    snap_br_env: disk_snap::Env<H>,
    abort_last_req: Arc<Mutex<Option<AbortHandle>>>,
}

impl<H> Service<H>
where
    H: SnapshotBrHandle,
{
    /// Create a new backup service.
    pub fn new(scheduler: Scheduler<Task>, env: disk_snap::Env<H>) -> Self {
        Service {
            scheduler,
            snap_br_env: env,
            abort_last_req: Arc::default(),
        }
    }
}

impl<H> Backup for Service<H>
where
    H: SnapshotBrHandle + 'static,
{
    /// Check a region whether there is pending admin requests(including pending
    /// merging).
    ///
    /// In older versions of disk snapshot backup, this will be called after we
    /// paused all scheduler.
    ///
    /// This is kept for compatibility with previous versions.
    fn check_pending_admin_op(
        &mut self,
        ctx: RpcContext<'_>,
        _req: CheckAdminRequest,
        mut sink: ServerStreamingSink<CheckAdminResponse>,
    ) {
        let handle = self.snap_br_env.handle.clone();
        let tokio_handle = self.snap_br_env.get_async_runtime().clone();
        let peer = ctx.peer();
        let task = async move {
            let (tx, rx) = mpsc::unbounded();
            if let Err(err) = handle.broadcast_check_pending_admin(tx) {
                return sink
                    .fail(RpcStatus::with_message(
                        RpcStatusCode::INTERNAL,
                        format!("{err}"),
                    ))
                    .await;
            }
            sink.send_all(&mut rx.map(|resp| Ok((resp, WriteFlags::default()))))
                .await?;
            sink.close().await?;
            Ok(())
        };

        tokio_handle.spawn(async move {
            match task.await {
                Err(err) => {
                    warn!("check admin canceled"; "peer" => %peer, "err" => %err);
                }
                Ok(()) => {
                    info!("check admin closed"; "peer" => %peer);
                }
            }
        });
    }

    fn backup(
        &mut self,
        ctx: RpcContext<'_>,
        req: BackupRequest,
        mut sink: ServerStreamingSink<BackupResponse>,
    ) {
        let mut cancel = None;
        // TODO: make it a bounded channel.
        let (tx, rx) = mpsc::unbounded();
        if let Err(status) = match Task::new(req, tx) {
            Ok((task, c)) => {
                cancel = Some(c);
                self.scheduler.schedule(task).map_err(|e| {
                    RpcStatus::with_message(RpcStatusCode::INVALID_ARGUMENT, format!("{:?}", e))
                })
            }
            Err(e) => Err(RpcStatus::with_message(
                RpcStatusCode::UNKNOWN,
                format!("{:?}", e),
            )),
        } {
            error!("backup task initiate failed"; "error" => ?status);
            ctx.spawn(
                sink.fail(status)
                    .unwrap_or_else(|e| error!("backup failed to send error"; "error" => ?e)),
            );
            return;
        };

        let send_task = async move {
            let mut s = rx.map(|resp| Ok((resp, WriteFlags::default())));
            sink.send_all(&mut s).await?;
            sink.close().await?;
            Ok(())
        }
        .map(|res: Result<()>| {
            match res {
                Ok(_) => {
                    info!("backup closed");
                }
                Err(e) => {
                    if let Some(c) = cancel {
                        // Cancel the running task.
                        c.store(true, Ordering::SeqCst);
                    }
                    error!("backup canceled"; "error" => ?e);
                }
            }
        });

        ctx.spawn(send_task);
    }

    /// The new method for preparing a disk snapshot backup.
    /// Generally there will be some steps for the client to do:
    /// 1. Establish a `prepare_snapshot_backup` connection.
    /// 2. Send a initial `UpdateLease`. And we should update the lease
    ///    periodically.
    /// 3. Send `WaitApply` to each leader peer in this store.
    /// 4. Once `WaitApply` for all regions have done, we can take disk
    ///    snapshot.
    /// 5. Once all snapshots have been taken, send `Finalize` to stop.
    fn prepare_snapshot_backup(
        &mut self,
        ctx: grpcio::RpcContext<'_>,
        stream: grpcio::RequestStream<PrepareSnapshotBackupRequest>,
        sink: grpcio::DuplexSink<PrepareSnapshotBackupResponse>,
    ) {
        let (l, new_cancel) = StreamHandleLoop::new(self.snap_br_env.clone());
        let peer = ctx.peer();
        // Note: should we disconnect here once there are more than one stream...?
        // Generally once two streams enter here, one may exit
        info!("A new prepare snapshot backup stream created!";
            "peer" => %peer,
            "stream_count" => %self.snap_br_env.active_stream(),
        );
        let abort_last_req = self.abort_last_req.clone();
        self.snap_br_env.get_async_runtime().spawn(async move {
            {
                let mut lock = abort_last_req.lock().unwrap();
                if let Some(cancel) = &*lock {
                    cancel.abort();
                }
                *lock = Some(new_cancel);
            }
            let res = l.run(stream, sink.into()).await;
            info!("stream closed; probably everything is done or a problem cannot be retried happens"; 
                "result" => ?res, "peer" => %peer);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use external_storage::make_local_backend;
    use tikv::storage::txn::tests::{must_commit, must_prewrite_put};
    use tikv_util::worker::{ReceiverWrapper, dummy_scheduler};
    use txn_types::TimeStamp;

    use super::*;
    use crate::{disk_snap::Env, endpoint::tests::*};

    #[derive(Clone)]
    struct PanicHandle;

    impl SnapshotBrHandle for PanicHandle {
        fn send_wait_apply(
            &self,
            _region: u64,
            _req: raftstore::store::snapshot_backup::SnapshotBrWaitApplyRequest,
        ) -> raftstore::Result<()> {
            panic!("this case shouldn't call this!")
        }

        fn broadcast_wait_apply(
            &self,
            _req: raftstore::store::snapshot_backup::SnapshotBrWaitApplyRequest,
        ) -> raftstore::Result<()> {
            panic!("this case shouldn't call this!")
        }

        fn broadcast_check_pending_admin(
            &self,
            _tx: mpsc::UnboundedSender<CheckAdminResponse>,
        ) -> raftstore::Result<()> {
            panic!("this case shouldn't call this!")
        }
    }

    fn new_rpc_suite() -> (Server, BackupClient, ReceiverWrapper<Task>) {
        let env = Arc::new(EnvBuilder::new().build());
        let (scheduler, rx) = dummy_scheduler();
        let backup_service =
            super::Service::new(scheduler, Env::new(PanicHandle, Default::default(), None));
        let builder =
            ServerBuilder::new(env.clone()).register_service(create_backup(backup_service));
        let mut server = builder.bind("127.0.0.1", 0).build().unwrap();
        server.start();
        let (_, port) = server.bind_addrs().next().unwrap();
        let addr = format!("127.0.0.1:{}", port);
        let channel = ChannelBuilder::new(env).connect(&addr);
        let client = BackupClient::new(channel);
        (server, client, rx)
    }

    #[test]
    fn test_client_stop() {
        let (_server, client, mut rx) = new_rpc_suite();

        let (tmp, endpoint) = new_endpoint();
        let mut engine = endpoint.engine.clone();
        endpoint.region_info.set_regions(vec![
            (b"".to_vec(), b"2".to_vec(), 1),
            (b"2".to_vec(), b"5".to_vec(), 2),
        ]);

        let mut ts: TimeStamp = 1.into();
        let mut alloc_ts = || *ts.incr();
        for i in 0..5 {
            let start = alloc_ts();
            let key = format!("{}", i);
            must_prewrite_put(
                &mut engine,
                key.as_bytes(),
                key.as_bytes(),
                key.as_bytes(),
                start,
            );
            let commit = alloc_ts();
            must_commit(&mut engine, key.as_bytes(), start, commit);
        }

        let now = alloc_ts();
        let mut req = BackupRequest::default();
        req.set_start_key(vec![]);
        req.set_end_key(vec![b'5']);
        req.set_start_version(now.into_inner());
        req.set_end_version(now.into_inner());
        // Set an unique path to avoid AlreadyExists error.
        req.set_storage_backend(make_local_backend(&tmp.path().join(now.to_string())));

        let stream = client.backup(&req).unwrap();
        let task = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Drop stream without start receiving will cause cancel error.
        drop(stream);
        // A stopped remote must not cause panic.
        endpoint.handle_backup_task(task.unwrap());

        // Set an unique path to avoid AlreadyExists error.
        req.set_storage_backend(make_local_backend(&tmp.path().join(alloc_ts().to_string())));
        let mut stream = client.backup(&req).unwrap();
        // Drop steam once it received something.
        client.spawn(async move {
            let _ = stream.next().await;
        });
        let task = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A stopped remote must not cause panic.
        endpoint.handle_backup_task(task.unwrap());

        // Set an unique path to avoid AlreadyExists error.
        req.set_storage_backend(make_local_backend(&tmp.path().join(alloc_ts().to_string())));
        let stream = client.backup(&req).unwrap();
        let task = rx.recv().unwrap();
        // Drop stream without start receiving will cause cancel error.
        drop(stream);
        // Wait util the task is canceled in map_err.
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if task.resp.unbounded_send(Default::default()).is_err() {
                break;
            }
        }
        // The task should be canceled.
        assert!(task.has_canceled());
        // A stopped remote must not cause panic.
        endpoint.handle_backup_task(task);
    }
}
