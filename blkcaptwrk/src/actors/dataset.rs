use super::{
    localsender::{LocalSenderActor, LocalSenderFinishedMessage},
    observation::observable_func,
    pool::PoolActor,
};
use crate::{
    actorbase::schedule_next_message,
    actorbase::unhandled_error,
    snapshots::PruneMessage,
    snapshots::{failed_snapshot_deletes_as_result, prune_btrfs_snapshots},
    xactorext::{join_all_actors, stop_all_actors, BoxBcWeakAddr, GetActorStatusMessage, TerminalState},
};
use crate::{
    actorbase::unhandled_result,
    xactorext::{BcActor, BcActorCtrl, BcHandler},
};
use anyhow::{Context as AnyhowContext, Result};
use cron::Schedule;
use futures_util::future::ready;
use libblkcapt::{
    core::{BtrfsDataset, BtrfsDatasetSnapshot, BtrfsPool, BtrfsSnapshot},
    core::{Snapshot, SnapshotHandle},
    model::entities::BtrfsDatasetEntity,
    model::entities::FeatureState,
    model::entities::ObservableEvent,
    model::Entity,
};
use slog::{info, o, Logger};
use std::{convert::TryInto, iter::once, path::PathBuf, sync::Arc};
use uuid::Uuid;
use xactor::{message, Actor, Addr, Context, Handler, Sender};

pub struct DatasetActor {
    pool: Addr<BcActor<PoolActor>>,
    dataset: Arc<BtrfsDataset>,
    snapshots: Vec<BtrfsDatasetSnapshot>,
    snapshot_schedule: Option<Schedule>,
    prune_schedule: Option<Schedule>,
    active_sends_holds: Vec<(BoxBcWeakAddr, Uuid, Option<Uuid>)>,
}

#[message()]
#[derive(Clone)]
struct SnapshotMessage();

#[message(result = "DatasetSnapshotsResponse")]
pub struct GetDatasetSnapshotsMessage;

pub struct DatasetSnapshotsResponse {
    pub snapshots: Vec<SnapshotHandle>,
}

#[message(result = "Result<()>")]
pub struct GetSnapshotSenderMessage {
    pub send_snapshot_handle: SnapshotHandle,
    pub parent_snapshot_handle: Option<SnapshotHandle>,
    pub target_ready: Sender<SenderReadyMessage>,
    pub target_finished: Sender<LocalSenderFinishedMessage>,
}

impl GetSnapshotSenderMessage {
    pub fn new<A>(
        requestor_addr: &Addr<A>, send_snapshot_handle: SnapshotHandle, parent_snapshot_handle: Option<SnapshotHandle>,
    ) -> Self
    where
        A: Handler<SenderReadyMessage> + Handler<LocalSenderFinishedMessage>,
    {
        Self {
            send_snapshot_handle,
            parent_snapshot_handle,
            target_ready: requestor_addr.sender(),
            target_finished: requestor_addr.sender(),
        }
    }
}

#[message()]
pub struct SenderReadyMessage(pub Result<Addr<BcActor<LocalSenderActor>>>);

#[message(result = "Result<()>")]
pub struct GetSnapshotHolderMessage {
    pub send_snapshot_handle: SnapshotHandle,
    pub parent_snapshot_handle: Option<SnapshotHandle>,
    pub target_ready: Sender<HolderReadyMessage>,
}

impl GetSnapshotHolderMessage {
    pub fn new<A>(
        requestor_addr: &Addr<A>, send_snapshot_handle: SnapshotHandle, parent_snapshot_handle: Option<SnapshotHandle>,
    ) -> Self
    where
        A: Handler<HolderReadyMessage>,
    {
        Self {
            send_snapshot_handle,
            parent_snapshot_handle,
            target_ready: requestor_addr.sender(),
        }
    }
}

#[message()]
pub struct HolderReadyMessage {
    pub holder: Result<Addr<BcActor<DatasetHolderActor>>>,
    pub snapshot_path: PathBuf,
    pub parent_snapshot_path: Option<PathBuf>,
}

impl DatasetActor {
    pub fn new(
        pool_actor: Addr<BcActor<PoolActor>>, pool: &Arc<BtrfsPool>, model: BtrfsDatasetEntity, log: &Logger,
    ) -> Result<BcActor<DatasetActor>> {
        let id = model.id();
        BtrfsDataset::validate(pool, model).map(Arc::new).and_then(|dataset| {
            Ok(BcActor::new(
                DatasetActor {
                    pool: pool_actor,
                    snapshots: dataset.snapshots()?,
                    dataset,
                    snapshot_schedule: None,
                    prune_schedule: None,
                    active_sends_holds: Default::default(),
                },
                &log.new(o!("dataset_id" => id.to_string())),
            ))
        })
    }

    fn schedule_next_snapshot(&self, log: &Logger, ctx: &mut Context<BcActor<Self>>) {
        schedule_next_message(self.snapshot_schedule.as_ref(), "snapshot", SnapshotMessage(), log, ctx);
    }

    fn schedule_next_prune(&self, log: &Logger, ctx: &mut Context<BcActor<Self>>) {
        schedule_next_message(self.prune_schedule.as_ref(), "prune", PruneMessage(), log, ctx);
    }
}

#[async_trait::async_trait]
impl BcActorCtrl for DatasetActor {
    async fn started(&mut self, log: &Logger, ctx: &mut Context<BcActor<Self>>) -> Result<()> {
        if self.dataset.model().snapshotting_state() == FeatureState::Enabled {
            self.snapshot_schedule = self
                .dataset
                .model()
                .snapshot_schedule
                .as_ref()
                .map_or(Ok(None), |s| s.try_into().map(Some))?;

            self.schedule_next_snapshot(log, ctx);
        }

        if self.dataset.model().pruning_state() == FeatureState::Enabled {
            self.prune_schedule = self
                .dataset
                .model()
                .snapshot_retention
                .as_ref()
                .map(|r| &r.evaluation_schedule)
                .map_or(Ok(None), |s| s.try_into().map(Some))?;

            self.schedule_next_prune(log, ctx);
        }

        Ok(())
    }

    async fn stopped(&mut self, _log: &Logger, _ctx: &mut Context<BcActor<Self>>) -> TerminalState {
        let mut active_actors = self
            .active_sends_holds
            .drain(..)
            .filter_map(|(actor, ..)| actor.upgrade())
            .collect::<Vec<_>>();
        if !active_actors.is_empty() {
            stop_all_actors(&mut active_actors);
            join_all_actors(active_actors).await;
            TerminalState::Cancelled
        } else {
            TerminalState::Succeeded
        }
    }
}

#[async_trait::async_trait]
impl BcHandler<SnapshotMessage> for DatasetActor {
    async fn handle(&mut self, log: &Logger, ctx: &mut Context<BcActor<Self>>, _msg: SnapshotMessage) {
        let result = observable_func(self.dataset.model().id(), ObservableEvent::DatasetSnapshot, || {
            ready(self.dataset.create_local_snapshot())
        })
        .await;
        match result {
            Ok(snapshot) => {
                info!(log, "snapshot created"; "time" => %snapshot.datetime());
                self.snapshots.push(snapshot);
            }
            Err(e) => {
                unhandled_error(log, e);
            }
        }

        self.schedule_next_snapshot(log, ctx);
    }
}

#[async_trait::async_trait]
impl BcHandler<PruneMessage> for DatasetActor {
    async fn handle(&mut self, log: &Logger, _ctx: &mut Context<BcActor<Self>>, _msg: PruneMessage) {
        let result = observable_func(self.dataset.model().id(), ObservableEvent::DatasetPrune, || {
            let rules = self
                .dataset
                .model()
                .snapshot_retention
                .as_ref()
                .expect("retention exist based on message scheduling in started");

            let holds: Vec<_> = self
                .active_sends_holds
                .iter()
                .flat_map(|a| once(a.1).chain(a.2.into_iter()))
                .collect();
            let failed_deletes = prune_btrfs_snapshots(&mut self.snapshots, &holds, rules, log);
            ready(failed_snapshot_deletes_as_result(failed_deletes))
        })
        .await;

        unhandled_result(log, result);
    }
}

#[async_trait::async_trait]
impl BcHandler<GetDatasetSnapshotsMessage> for DatasetActor {
    async fn handle(
        &mut self, _log: &Logger, _ctx: &mut Context<BcActor<Self>>, _msg: GetDatasetSnapshotsMessage,
    ) -> DatasetSnapshotsResponse {
        DatasetSnapshotsResponse {
            snapshots: self.snapshots.iter().map(|s| s.into()).collect(),
        }
    }
}

#[async_trait::async_trait]
impl BcHandler<GetSnapshotSenderMessage> for DatasetActor {
    async fn handle(
        &mut self, log: &Logger, ctx: &mut Context<BcActor<Self>>, msg: GetSnapshotSenderMessage,
    ) -> Result<()> {
        let send_snapshot = self
            .snapshots
            .iter()
            .find(|s| s.uuid() == msg.send_snapshot_handle.uuid)
            .context("Snapshot not found.")?;
        let parent_snapshot = match msg.parent_snapshot_handle {
            Some(handle) => Some(
                self.snapshots
                    .iter()
                    .find(|s| s.uuid() == handle.uuid)
                    .context("Parent not found")?,
            ),
            None => None,
        };

        let snapshot_sender = send_snapshot.send(parent_snapshot);
        let started_sender_actor = LocalSenderActor::new(
            ctx.address().sender(),
            msg.target_finished,
            snapshot_sender,
            &log.new(o!("message" => ())),
        )
        .start()
        .await;

        if let Ok(addr) = &started_sender_actor {
            self.active_sends_holds
                .push((addr.into(), send_snapshot.uuid(), parent_snapshot.map(|s| s.uuid())));
        }
        msg.target_ready.send(SenderReadyMessage(started_sender_actor))?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl BcHandler<GetSnapshotHolderMessage> for DatasetActor {
    async fn handle(
        &mut self, log: &Logger, ctx: &mut Context<BcActor<Self>>, msg: GetSnapshotHolderMessage,
    ) -> Result<()> {
        let send_snapshot = self
            .snapshots
            .iter()
            .find(|s| s.uuid() == msg.send_snapshot_handle.uuid)
            .context("Snapshot not found.")?;
        let parent_snapshot = match &msg.parent_snapshot_handle {
            Some(handle) => Some(
                self.snapshots
                    .iter()
                    .find(|s| s.uuid() == handle.uuid)
                    .context("Parent not found")?,
            ),
            None => None,
        };

        let started_holder_actor = DatasetHolderActor::new(
            log,
            ctx.address().sender(),
            msg.send_snapshot_handle,
            msg.parent_snapshot_handle,
        )
        .start()
        .await;
        if let Ok(addr) = &started_holder_actor {
            self.active_sends_holds
                .push((addr.into(), send_snapshot.uuid(), parent_snapshot.map(|s| s.uuid())));
        }
        msg.target_ready.send(HolderReadyMessage {
            holder: started_holder_actor,
            snapshot_path: send_snapshot.canonical_path(),
            parent_snapshot_path: parent_snapshot.map(|s| s.canonical_path()),
        })?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl BcHandler<LocalSenderFinishedMessage> for DatasetActor {
    async fn handle(&mut self, _log: &Logger, _ctx: &mut Context<BcActor<Self>>, msg: LocalSenderFinishedMessage) {
        self.active_sends_holds.retain(|(x, ..)| x.actor_id() != msg.0);
    }
}

#[async_trait::async_trait]
impl BcHandler<GetActorStatusMessage> for DatasetActor {
    async fn handle(
        &mut self, _log: &Logger, _ctx: &mut Context<BcActor<Self>>, _msg: GetActorStatusMessage,
    ) -> String {
        String::from("ok")
    }
}

pub struct DatasetHolderActor {
    parent: Sender<LocalSenderFinishedMessage>,
}

impl DatasetHolderActor {
    fn new(
        log: &Logger, parent: Sender<LocalSenderFinishedMessage>, send_handle: SnapshotHandle,
        parent_handle: Option<SnapshotHandle>,
    ) -> BcActor<DatasetHolderActor> {
        let snapshot_id = send_handle.uuid.to_string();
        let log = match parent_handle {
            Some(parent) => {
                log.new(o!("snapshot_pinned" => snapshot_id, "snapshot_parent_pinned" => parent.uuid.to_string()))
            }
            None => log.new(o!("snapshot_pinned" => snapshot_id)),
        };
        BcActor::new(DatasetHolderActor { parent }, &log)
    }
}

#[async_trait::async_trait]
impl BcActorCtrl for DatasetHolderActor {
    async fn stopped(&mut self, _log: &Logger, ctx: &mut Context<BcActor<Self>>) -> TerminalState {
        let _ = self.parent.send(LocalSenderFinishedMessage(ctx.actor_id(), Ok(())));
        TerminalState::Succeeded
    }
}

#[async_trait::async_trait]
impl BcHandler<GetActorStatusMessage> for DatasetHolderActor {
    async fn handle(
        &mut self, _log: &Logger, _ctx: &mut Context<BcActor<Self>>, _msg: GetActorStatusMessage,
    ) -> String {
        String::from("ok")
    }
}
