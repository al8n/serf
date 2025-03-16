use std::sync::Arc;

use async_channel::Sender;
use serf_core::{delegate::Delegate, event::Event, transport::Transport};

use crate::event_handler::EventFilter;

pub(super) struct EventStream<C, T, D>
where
  T: Transport,
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
{
  client: C,
  event_tx: Sender<Event<T, D>>,
  filters: Arc<[EventFilter]>,
}

