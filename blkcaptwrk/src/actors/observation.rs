use crate::{
    actorbase::unhandled_result,
    xactorext::{BcActor, BcActorCtrl, BcHandler},
};
use anyhow::Result;
use libblkcapt::{
    core::ObservableEventStage,
    core::ObservationEmitter,
    core::ObservationRouter,
    model::entities::HealthchecksHeartbeat,
    model::entities::{HealthchecksObserverEntity, ObservableEvent},
    model::Entity,
};
use slog::{o, Logger};
use std::{fmt::Debug, future::Future};
use uuid::Uuid;
use xactor::{message, Broker, Context, Service};

#[message()]
#[derive(Clone, Debug)]
pub struct ObservableEventMessage {
    pub source: Uuid,
    pub event: ObservableEvent,
    pub stage: ObservableEventStage,
}

#[message()]
#[derive(Clone)]
struct HeartbeatMessage();

pub async fn observable_func<F, T, E, R>(source: Uuid, event: ObservableEvent, func: F) -> std::result::Result<T, E>
where
    F: FnOnce() -> R,
    R: Future<Output = std::result::Result<T, E>>,
    E: Debug,
{
    let mut broker = Broker::from_registry().await.expect("broker is always available");
    broker
        .publish(ObservableEventMessage {
            source,
            event,
            stage: ObservableEventStage::Starting,
        })
        .expect("can always publish");

    let result = func().await;

    let final_stage = match &result {
        Ok(_) => {
            slog_scope::trace!("observable func succeeded"; "entity_id" => %source, "observable_event" => %event);
            ObservableEventStage::Succeeded
        }
        Err(e) => {
            slog_scope::trace!("observable func failed"; "entity_id" => %source, "observable_event" => %event, "error" => ?e);
            ObservableEventStage::Failed(format!("{:?}", e))
        }
    };

    broker
        .publish(ObservableEventMessage {
            source,
            event,
            stage: final_stage,
        })
        .expect("can always publish");

    result
}

pub struct HealthchecksActor {
    router: ObservationRouter,
    emitter: ObservationEmitter,
    heartbeat_config: Option<HealthchecksHeartbeat>,
}

impl HealthchecksActor {
    pub fn new(model: HealthchecksObserverEntity, log: &Logger) -> BcActor<Self> {
        let observer_id = model.id().to_string();
        BcActor::new(
            Self {
                router: ObservationRouter::new(model.observations),
                emitter: model
                    .custom_url
                    .map_or_else(ObservationEmitter::default, ObservationEmitter::new),
                heartbeat_config: model.heartbeat,
            },
            &log.new(o!("observer_id" => observer_id)),
        )
    }
}

#[async_trait::async_trait]
impl BcActorCtrl for HealthchecksActor {
    async fn started(&mut self, _log: &Logger, ctx: &mut Context<BcActor<Self>>) -> Result<()> {
        ctx.subscribe::<ObservableEventMessage>().await?;

        if let Some(config) = &self.heartbeat_config {
            ctx.address().send(HeartbeatMessage())?;
            ctx.send_interval(HeartbeatMessage(), config.frequency);
        }

        Ok(())
    }

    async fn stopped(&mut self, _log: &Logger, ctx: &mut Context<BcActor<Self>>) {
        ctx.unsubscribe::<ObservableEventMessage>()
            .await
            .expect("can always unsubscribe");
    }
}

#[async_trait::async_trait]
impl BcHandler<ObservableEventMessage> for HealthchecksActor {
    async fn handle(&mut self, log: &Logger, _ctx: &mut Context<BcActor<Self>>, msg: ObservableEventMessage) {
        let observers = self.router.route(msg.source, msg.event);
        for observer in observers {
            let result = self.emitter.emit(observer.healthcheck_id, msg.stage.clone()).await;
            unhandled_result(log, result);
        }
    }
}

#[async_trait::async_trait]
impl BcHandler<HeartbeatMessage> for HealthchecksActor {
    async fn handle(&mut self, log: &Logger, _ctx: &mut Context<BcActor<Self>>, _msg: HeartbeatMessage) {
        let result = self
            .emitter
            .emit(
                self.heartbeat_config
                    .as_ref()
                    .expect("heartbeat config exists if heartbeat messages are scheduled")
                    .healthcheck_id,
                ObservableEventStage::Succeeded,
            )
            .await;

        unhandled_result(log, result);
    }
}
