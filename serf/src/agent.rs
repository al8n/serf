use std::{collections::HashMap, sync::Arc};

use agnostic::RuntimeLite;
use futures::FutureExt;
use parking_lot::Mutex;
use serf_core::{
  delegate::Delegate, transport::Transport, types::{bytes::Bytes, MaybeResolvedAddress, Node, SmallVec, Tags}, Options, QueryParam, QueryResponse, Serf
};
#[cfg(feature = "encryption")]
use serf_core::{types::SecretKey, key_manager::KeyResponse};
use smol_str::SmolStr;

/// The event handler for `Serf` agent.
pub mod event_handler;

/// The options for [`Agent`].
pub mod options;

/// The error for the [`Agent`].
pub mod error;

mod invoke;
mod mdns;

/// Profile is used to control the timing profiles used in `Serf`.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[cfg_attr(feature = "cli", clap(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum Profile {
  /// Lan is used for local area networks.
  #[default]
  Lan,
  /// Wan is used for wide area networks.
  Wan,
  /// Local is used for local.
  Local,
}

struct EventHandlerRegistry<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  handlers: HashMap<SmolStr, Arc<dyn event_handler::EventHandler<T, D>>>,
  handlers_list: Arc<[Arc<dyn event_handler::EventHandler<T, D>>]>,
}

pub struct Agent<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  // Stores the serf configuration
  conf: Options,

  // Stores the agent configuration
  agent_conf: options::AgentOptions<T::Id, T::ResolvedAddress>,

  serf: Serf<T, D>,

  event_registry: Arc<Mutex<EventHandlerRegistry<T, D>>>,

  shutdown_tx: async_channel::Sender<()>,
  shutdown_rx: async_channel::Receiver<()>,
}

impl<T, D> Agent<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  /// Returns a channel that can be selected to wait
  /// for the agent to perform a shutdown.
  pub fn shutdown_rx(&self) -> async_channel::Receiver<()> {
    self.shutdown_rx.clone()
  }

  /// Returns the [`Serf`] agent of the running [`Agent`].
  #[inline]
  pub const fn serf(&self) -> &Serf<T, D> {
    &self.serf
  }

  /// Returns the [`Serf`]'s options of the running [`Agent`].
  #[inline]
  pub const fn serf_options(&self) -> &Options {
    &self.conf
  }

  /// Used to eject a failed node from the cluster
  pub async fn force_leave(&self, node: T::Id) -> Result<(), error::Error<T, D>> {
    tracing::info!(node=%node, "serf agent: force leaving node");
    self.serf.remove_failed_node(node).await.map_err(|e| {
      tracing::warn!(err=%e, "serf agent: failed to remove node");

      e.into()
    })
  }

  /// Completely removes a failed node from the memberlist entirely
  pub async fn force_leave_prune(&self, node: T::Id) -> Result<(), error::Error<T, D>> {
    tracing::info!(node=%node, "serf agent: force leaving node (prune)");
    self.serf.remove_failed_node_prune(node).await.map_err(|e| {
      tracing::warn!(err=%e, "serf agent: failed to remove node (prune)");

      e.into()
    })
  }

  /// Prepares for a graceful shutdown of the agent and its processes
  pub async fn leave(&self) -> Result<(), error::Error<T, D>> {
    tracing::info!("serf agent: requesting graceful leave from Serf");
    self.serf.leave().await.map_err(Into::into)
  }

  /// Closes this agent and all of its processes. Should be preceded
  /// by a Leave for a graceful shutdown.
  pub async fn shutdown(&self) -> Result<(), error::Error<T, D>> {
    if !self.shutdown_tx.close() {
      return Ok(());
    }

    tracing::info!("serf agent: requesting serf shutdown");
    self.serf.shutdown().await?;

    tracing::info!("serf agent: shutdown complete");
    Ok(())
  }

  /// Sends a user event on [`Serf`], see [`Serf::user_event`]
  pub async fn user_event(
    &self,
    name: impl Into<SmolStr>,
    payload: impl Into<Bytes>,
    coalesce: bool,
  ) -> Result<(), error::Error<T, D>> {
    let payload = payload.into();
    tracing::debug!(coalesce=%coalesce, payload=?payload.as_ref(), "serf agent: requesting user event send");
    self
      .serf
      .user_event(name, payload, coalesce)
      .await
      .map_err(|e| {
        tracing::warn!(err=%e, "serf agent: failed to send user event");
        e.into()
      })
  }

  /// Sends a query event on [`Serf`], see [`Serf::query`]
  pub async fn query(
    &self,
    name: impl Into<SmolStr>,
    payload: impl Into<Bytes>,
    params: Option<QueryParam<T::Id>>,
  ) -> Result<QueryResponse<T::Id, T::ResolvedAddress>, error::Error<T, D>> {
    let name = name.into();
    let payload = payload.into();

    // Prevent the use of the internal prefix
    if name.starts_with(serf_core::event::INTERNAL_QUERY_PREFIX) {
      // Allow the special "ping" query
      if name != "_serf_ping" || !payload.is_empty() {
        return Err(error::Error::InternalQueryPrefix);
      }
    }

    tracing::debug!(params=?params, payload=?payload.as_ref(), "serf agent: requesting query send");

    self.serf.query(name, payload, params).await.map_err(|e| {
      tracing::warn!(err=%e, "serf agent: failed to start user query");
      e.into()
    })
  }

  /// Asks the Serf instance to join. See the [`Serf::join`] function.
  pub async fn join(
    &self,
    existing: impl Iterator<Item = Node<T::Id, MaybeResolvedAddress<T::Address, T::ResolvedAddress>>>,
    replay: bool,
  ) -> Result<
    SmallVec<Node<T::Id, T::ResolvedAddress>>,
    (
      SmallVec<Node<T::Id, T::ResolvedAddress>>,
      error::Error<T, D>,
    ),
  > {
    tracing::info!(replay=%replay, "serf agent: requesting join");

    self
      .serf
      .join_many(existing, !replay)
      .await
      .inspect(|joined| {
        if !joined.is_empty() {
          tracing::info!("serf agent: joined {} nodes", joined.len());
        }
      })
      .map_err(|(joined, e)| {
        if !joined.is_empty() {
          tracing::info!("serf agent: joined {} nodes", joined.len());
        }

        tracing::warn!(err=%e, "serf agent: error joining");
        (joined, e.into())
      })
  }

  /// Adds an event handler to receive event notifications
  pub fn register_event_handler(
    &self,
    handler: impl event_handler::EventHandler<T, D>,
  ) -> Result<(), error::Error<T, D>> {
    let mut registry = self.event_registry.lock();
    let name = handler.name().clone();
    if registry.handlers.contains_key(&name) {
      return Err(error::Error::DuplicatedEventHandler(name));
    }

    registry.handlers.insert(
      name.clone(),
      Arc::from(Box::new(handler) as Box<dyn event_handler::EventHandler<T, D>>),
    );
    registry.handlers_list = registry.handlers.values().cloned().collect();
    Ok(())
  }

  /// Removes an event handler from the event registry and prevents more invocations
  pub fn deregister_event_handler(&self, name: &str) {
    let mut registry = self.event_registry.lock();
    registry.handlers.remove(name);
    registry.handlers_list = registry.handlers.values().cloned().collect();
  }

  /// Used to update the tags. The agent will make sure to
  /// persist tags if necessary before gossiping to the cluster.
  pub async fn update_tags(
    &self,
    tags: Tags,
  ) -> Result<(), error::Error<T, D>> {
    // Update the tags file if we have one
    if let Some(f) = self.agent_conf.tags_file() {
      
    }

    self.serf.update_tags(tags).await.map_err(Into::into)
  }

  /// Initiates a query to install a new key on all members
  #[cfg(feature = "encryption")]
  pub async fn install_key(
    &self,
    key: SecretKey,
  ) -> Result<KeyResponse<T::Id>, error::Error<T, D>> {
    tracing::info!("serf agent: initiating key installation");

    self.serf.key_manager().install_key(key, None).await.map_err(Into::into)
  }

  /// Sends a query instructing all members to switch primary keys
  #[cfg(feature = "encryption")]
  pub async fn use_key(
    &self,
    key: SecretKey,
  ) -> Result<KeyResponse<T::Id>, error::Error<T, D>> {
    tracing::info!("serf agent: initiating primary key change");

    self.serf.key_manager().use_key(key, None).await.map_err(Into::into)
  }

  /// Sends a query instructing all members to remove a key from the keyring
  #[cfg(feature = "encryption")]
  pub async fn remove_key(
    &self,
    key: SecretKey,
  ) -> Result<KeyResponse<T::Id>, error::Error<T, D>> {
    tracing::info!("serf agent: initiating key removal");

    self.serf.key_manager().remove_key(key, None).await.map_err(Into::into)
  }

  /// Sends a query to all members to return a list of their keys
  #[cfg(feature = "encryption")]
  pub async fn list_keys(&self) -> Result<KeyResponse<T::Id>, error::Error<T, D>> {
    tracing::info!("serf agent: initiating key listing");

    self.serf.key_manager().list_keys().await.map_err(Into::into)
  }

  async fn event_loop(
    registry: Arc<Mutex<EventHandlerRegistry<T, D>>>,
    event_rx: async_channel::Receiver<serf_core::event::Event<T, D>>,
    serf_shutdown_rx: async_channel::Receiver<()>,
    shutdown_rx: async_channel::Receiver<()>,
  ) {
    loop {
      futures::select! {
        _ = shutdown_rx.recv().fuse() => {
          break;
        }
        _ = serf_shutdown_rx.recv().fuse() => {
          tracing::warn!("serf agent: serf shutdown detected, quitting");
          break;
        }
        event = event_rx.recv().fuse() => {
          let event = match event {
            Ok(event) => event,
            Err(e) => {
              tracing::error!(err=%e, "serf agent: fail to receive event");
              continue;
            }
          };

          let handlers = {
            let registry = registry.lock();
            registry.handlers_list.clone()
          };

          for handler in handlers.iter() {
            // TODO(al8n): spawn the handler in a separate task?
            handler.handle(&event).await;
          }
        }
      }
    }
  }
}

fn load_tags_file(file: &std::path::Path) -> std::io::Result<Tags> {
  todo!()
}

fn write_tags_file(
  tags: &Tags,
  file: &std::path::Path,
) -> std::io::Result<()> {
  use std::{fs::OpenOptions, io::Write};

  fn encode(buf: &mut [u8], tags: &Tags) -> usize {
    let mut offset = 0;
    for (k, v) in tags.iter() {
      buf[offset..offset + k.len()].copy_from_slice(k.as_bytes());
      offset += k.len();
      buf[offset] = b'=';
      offset += 1;

      buf[offset..offset + v.len()].copy_from_slice(v.as_bytes());
      offset += v.len();
      buf[offset] = b'\n';
      offset += 1;
    }

    #[cfg(debug_assertions)]
    {
      assert_eq!(offset, buf.len(), "expect writting {} bytes, but actual write {} bytes", buf.len(), offset);
    }

    offset
  }

  let mut opts = OpenOptions::new();
  opts.write(true).create(true).truncate(true);

  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
  }

  let encoded_len = tags.iter().fold(0usize, |acc, (k, v)| {
    // key=value\n
    acc + (k.len() + v.len() + 2)
  });

  let mut file = opts.open(file)?;

  if encoded_len > 1024 {
    let mut buffer = vec![0; encoded_len];
    let len = encode(&mut buffer, tags);
    file.write_all(&buffer[..len])?;
  } else {
    let mut buffer = [0; 1024];
    let len = encode(&mut buffer[..encoded_len], tags);
    file.write_all(&buffer[..len])?;
  }

  Ok(())
}
