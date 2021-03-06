// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::metadata::Metadata;
use crate::module::pubsub::event_subscription_actor::ChainNotifyHandlerActor;
use crate::module::pubsub::notify::SubscriberNotifyActor;
use actix::Addr;
use futures::channel::mpsc;
use jsonrpc_core::Result;
use jsonrpc_pubsub::typed::Subscriber;
use jsonrpc_pubsub::SubscriptionId;
use parking_lot::RwLock;
use starcoin_bus::BusActor;

use starcoin_rpc_api::types::pubsub::EventFilter;
use starcoin_rpc_api::{errors, pubsub::StarcoinPubSub, types::pubsub};
use starcoin_storage::Store;
use starcoin_txpool_api::TxnStatusFullEvent;
use starcoin_types::filter::Filter;

use std::convert::TryInto;
use std::sync::{atomic, Arc};
use subscribers::Subscribers;
use txn_subscription_actor::TransactionSubscriptionActor;

mod event_subscription_actor;
mod notify;
mod subscribers;
#[cfg(test)]
pub mod tests;
mod txn_subscription_actor;

pub struct PubSubImpl {
    service: PubSubService,
}

impl PubSubImpl {
    pub fn new(s: PubSubService) -> Self {
        Self { service: s }
    }
}

impl StarcoinPubSub for PubSubImpl {
    type Metadata = Metadata;
    fn subscribe(
        &self,
        _meta: Metadata,
        subscriber: Subscriber<pubsub::Result>,
        kind: pubsub::Kind,
        params: Option<pubsub::Params>,
    ) {
        let error = match (kind, params) {
            (pubsub::Kind::NewHeads, None) => {
                self.service.add_new_header_subscription(subscriber);
                return;
            }
            (pubsub::Kind::NewHeads, _) => {
                errors::invalid_params("newHeads", "Expected no parameters.")
            }
            (pubsub::Kind::NewPendingTransactions, None) => {
                self.service.add_new_txn_subscription(subscriber);
                return;
            }
            (pubsub::Kind::NewPendingTransactions, _) => {
                errors::invalid_params("newPendingTransactions", "Expected no parameters.")
            }
            (pubsub::Kind::Events, Some(pubsub::Params::Events(filter))) => {
                self.service.add_event_subscription(subscriber, filter);
                return;
            }
            (pubsub::Kind::Events, _) => {
                errors::invalid_params("events", "Expected a filter object.")
            } // _ => errors::unimplemented(None),
        };

        let _ = subscriber.reject(error);
    }

    fn unsubscribe(&self, _: Option<Self::Metadata>, id: SubscriptionId) -> Result<bool> {
        self.service.unsubscribe(id)
    }
}

type ClientNotifier = Addr<SubscriberNotifyActor<pubsub::Result>>;
type TxnSubscribers = Arc<RwLock<Subscribers<ClientNotifier>>>;
type EventSubscribers = Arc<RwLock<Subscribers<(ClientNotifier, Filter)>>>;
type NewHeaderSubscribers = Arc<RwLock<Subscribers<ClientNotifier>>>;

pub struct PubSubService {
    subscriber_id: Arc<atomic::AtomicU64>,
    spawner: actix_rt::Arbiter,
    transactions_subscribers: TxnSubscribers,
    events_subscribers: EventSubscribers,
    new_header_subscribers: NewHeaderSubscribers,
}

impl Default for PubSubService {
    fn default() -> Self {
        Self::new()
    }
}

impl PubSubService {
    pub fn new() -> Self {
        let subscriber_id = Arc::new(atomic::AtomicU64::new(0));
        let transactions_subscribers =
            Arc::new(RwLock::new(Subscribers::new(subscriber_id.clone())));
        let events_subscribers = Arc::new(RwLock::new(Subscribers::new(subscriber_id.clone())));
        let new_header_subscribers = Arc::new(RwLock::new(Subscribers::new(subscriber_id.clone())));
        Self {
            spawner: actix_rt::Arbiter::new(),
            subscriber_id,
            transactions_subscribers,
            events_subscribers,
            new_header_subscribers,
        }
    }

    pub fn start_chain_notify_handler(&self, bus: Addr<BusActor>, store: Arc<dyn Store>) {
        let actor = ChainNotifyHandlerActor::new(
            self.events_subscribers.clone(),
            self.new_header_subscribers.clone(),
            bus,
            store,
        );
        actix::Actor::start_in_arbiter(&self.spawner, |_ctx| actor);
    }

    pub fn start_transaction_subscription_handler(
        &self,
        txn_receiver: mpsc::UnboundedReceiver<TxnStatusFullEvent>,
    ) {
        let actor =
            TransactionSubscriptionActor::new(self.transactions_subscribers.clone(), txn_receiver);

        actix::Actor::start_in_arbiter(&self.spawner, |_ctx| actor);
    }

    pub fn add_new_txn_subscription(&self, subscriber: Subscriber<pubsub::Result>) {
        self.transactions_subscribers
            .write()
            .add(&self.spawner, subscriber);
    }
    pub fn add_new_header_subscription(&self, subscriber: Subscriber<pubsub::Result>) {
        self.new_header_subscribers
            .write()
            .add(&self.spawner, subscriber);
    }
    pub fn add_event_subscription(
        &self,
        subscriber: Subscriber<pubsub::Result>,
        filter: EventFilter,
    ) {
        match filter.try_into() {
            Ok(f) => {
                self.events_subscribers
                    .write()
                    .add(&self.spawner, subscriber, f);
            }
            Err(e) => {
                let _ = subscriber.reject(e);
            }
        };
    }
    pub fn unsubscribe(&self, id: SubscriptionId) -> Result<bool> {
        let res1 = self.events_subscribers.write().remove(&id).is_some();
        let res2 = self.transactions_subscribers.write().remove(&id).is_some();
        let res3 = self.new_header_subscribers.write().remove(&id).is_some();
        Ok(res1 || res2 || res3)
    }
}
