use async_channel::{Receiver, Sender, bounded};
use futures::FutureExt;
use memberlist_core::{
  agnostic_lite::{AsyncSpawner, RuntimeLite},
  bytes::Bytes,
  tracing,
  transport::Transport,
};

use crate::{
  delegate::Delegate,
  event::{CrateEvent, InternalQueryEvent, QueryEvent},
};

#[cfg(feature = "encryption")]
use crate::{
  error::Error,
  types::{KeyResponseMessage, MessageRef},
};

#[cfg(feature = "encryption")]
use smol_str::SmolStr;

/// Used to compute the max number of keys in a list key
/// response. eg 1024/25 = 40. a message with max size of 1024 bytes cannot
/// contain more than 40 keys. There is a test
/// (TestSerfQueries_estimateMaxKeysInListKeyResponse) which does the
/// computation and in case of changes, the value can be adjusted.
#[cfg(feature = "encryption")]
const MIN_ENCODED_KEY_LENGTH: usize = 25;

pub(crate) struct SerfQueries<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  in_rx: Receiver<CrateEvent<T, D>>,
  out_tx: Option<Sender<CrateEvent<T, D>>>,
  shutdown_rx: Receiver<()>,
}

impl<D, T> SerfQueries<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  #[allow(clippy::new_ret_no_self)]
  pub(crate) fn new(
    out_tx: Option<Sender<CrateEvent<T, D>>>,
    shutdown_rx: Receiver<()>,
  ) -> (
    Sender<CrateEvent<T, D>>,
    <<T::Runtime as RuntimeLite>::Spawner as AsyncSpawner>::JoinHandle<()>,
  ) {
    let (in_tx, in_rx) = bounded(1024);
    let this = Self {
      in_rx,
      out_tx,
      shutdown_rx,
    };
    (in_tx, this.stream())
  }

  /// A long running routine to ingest the event stream
  fn stream(self) -> <<T::Runtime as RuntimeLite>::Spawner as AsyncSpawner>::JoinHandle<()> {
    <T::Runtime as RuntimeLite>::spawn(async move {
      loop {
        futures::select! {
          ev = self.in_rx.recv().fuse() => {
            match ev {
              Ok(ev) => {
                // Check if this is a query we should process
                if ev.is_internal_query() {
                  <T::Runtime as RuntimeLite>::spawn_detach(async move {
                    Self::handle_query(ev).await;
                  });
                } else if let Some(ref tx) = self.out_tx {
                  if let Err(e) = tx.send(ev).await {
                    tracing::error!(target="serf", err=%e, "failed to send event back in serf query thread");
                  }
                }
              },
              Err(err) => {
                tracing::error!(target="serf", err=%err, "failed to receive event in serf query thread");
                return;
              }
            }
          }
          _ = self.shutdown_rx.recv().fuse() => {
            return;
          }
        }
      }
    })
  }

  async fn handle_query(ev: CrateEvent<T, D>) {
    match ev {
      CrateEvent::InternalQuery { kind, query } => match kind {
        InternalQueryEvent::Ping => {}
        InternalQueryEvent::Conflict(conflict) => {
          Self::handle_conflict(&conflict, &query).await;
        }
        #[cfg(feature = "encryption")]
        InternalQueryEvent::InstallKey => {
          Self::handle_install_key(&query).await;
        }
        #[cfg(feature = "encryption")]
        InternalQueryEvent::UseKey => {
          Self::handle_use_key(&query).await;
        }
        #[cfg(feature = "encryption")]
        InternalQueryEvent::RemoveKey => {
          Self::handle_remove_key(&query).await;
        }
        #[cfg(feature = "encryption")]
        InternalQueryEvent::ListKey => {
          Self::handle_list_keys(&query).await;
        }
      },
      _ => unreachable!(),
    }
  }

  /// invoked when we get a query that is attempting to
  /// disambiguate a name conflict. They payload is a node name, and the response
  /// should the address we believe that node is at, if any.
  async fn handle_conflict(conflict: &T::Id, ev: &QueryEvent<T, D>) {
    // The target node id is the payload

    // Do not respond to the query if it is about us
    if conflict.eq(ev.ctx.this.inner.memberlist.local_id()) {
      return;
    }

    tracing::error!(
      "serf: local {} got conflict resolution query for '{}'",
      ev.ctx.this.inner.memberlist.local_id(),
      conflict
    );

    // tracing::debug!("serf: got conflict resolution query for '{}'", conflict);

    // Look for the member info
    let out = {
      let members = ev.ctx.this.inner.members.read().await;
      members.states.get(conflict).cloned()
    };

    // Encode the response
    match out {
      Some(state) => {
        let resp = crate::types::ConflictResponseMessageBorrow::from(state.member());
        match crate::types::encode_message_to_bytes(&resp) {
          Ok(raw) => {
            if let Err(e) = ev.respond(raw).await {
              tracing::error!(target="serf", err=%e, "failed to respond to conflict query");
            }
          }
          Err(e) => {
            tracing::error!(target="serf", err=%e, "failed to encode conflict query response");
          }
        }
      }
      None => {
        tracing::warn!("serf: no member status found for '{}'", conflict);
        // TODO: consider send something back?
        if let Err(e) = ev.respond(Bytes::new()).await {
          tracing::error!(target="serf", err=%e, "failed to respond to conflict query");
        }
      }
    }
  }

  /// Invoked whenever a new encryption key is received from
  /// another member in the cluster, and handles the process of installing it onto
  /// the memberlist keyring. This type of query may fail if the provided key does
  /// not fit the constraints that memberlist enforces. If the query fails, the
  /// response will contain the error message so that it may be relayed.
  #[cfg(feature = "encryption")]
  async fn handle_install_key(ev: impl AsRef<QueryEvent<T, D>> + Send) {
    let q = ev.as_ref();
    let mut response = KeyResponseMessage::default();
    let req = match crate::types::decode_message::<T::Id, T::ResolvedAddress>(&q.payload) {
      Ok(msg) => match msg {
        MessageRef::KeyRequest(req) => req,
        msg => {
          tracing::error!(
            err = "unexpected message type",
            "serf: {}",
            msg.ty().as_str()
          );
          Self::send_key_response(q, &mut response).await;
          return;
        }
      },
      Err(e) => {
        tracing::error!(err=%e, "serf: failed to decode key request");
        Self::send_key_response(q, &mut response).await;
        return;
      }
    };

    if !q.ctx.this.encryption_enabled() {
      tracing::error!(
        err = "encryption is not enabled",
        "serf: fail to handle install key"
      );
      response.message = SmolStr::new("encryption is not enabled");
      Self::send_key_response(q, &mut response).await;
      return;
    }

    tracing::info!("serf: received install-key query");
    let kr = q.ctx.this.inner.memberlist.keyring();
    match kr {
      Some(kr) => {
        kr.insert(req.key.unwrap());
        if q.ctx.this.inner.opts.keyring_file.is_some() {
          if let Err(e) = q.ctx.this.write_keyring_file().await {
            tracing::error!(err=%e, "serf: failed to write keyring file");
            response.message = SmolStr::new(e.to_string());
            Self::send_key_response(q, &mut response).await;
            return;
          }
        }

        response.result = true;
        Self::send_key_response(q, &mut response).await;
      }
      None => {
        tracing::error!(
          err = "encryption enabled but keyring is empty",
          "serf: fail to handle install key"
        );
        response.message = SmolStr::new("encryption enabled but keyring is empty");
        Self::send_key_response(q, &mut response).await;
      }
    }
  }

  #[cfg(feature = "encryption")]
  async fn handle_use_key(ev: impl AsRef<QueryEvent<T, D>> + Send) {
    let q = ev.as_ref();
    let mut response = KeyResponseMessage::default();

    let req = match crate::types::decode_message::<T::Id, T::ResolvedAddress>(&q.payload) {
      Ok(msg) => match msg {
        MessageRef::KeyRequest(req) => req,
        msg => {
          tracing::error!(err = "unexpected message type", "serf: {}", msg.ty());
          Self::send_key_response(q, &mut response).await;
          return;
        }
      },
      Err(e) => {
        tracing::error!(err=%e, "serf: failed to decode key request");
        Self::send_key_response(q, &mut response).await;
        return;
      }
    };

    if !q.ctx.this.encryption_enabled() {
      tracing::error!(
        err = "encryption is not enabled",
        "serf: fail to handle use key"
      );
      response.message = SmolStr::new("encryption is not enabled");
      Self::send_key_response(q, &mut response).await;
      return;
    }

    tracing::info!("serf: received use-key query");
    let kr = q.ctx.this.inner.memberlist.keyring();
    match kr {
      Some(kr) => {
        if let Err(e) = kr.use_key(&req.key.unwrap()) {
          tracing::error!(err=%e, "serf: failed to change primary key");
          response.message = SmolStr::new(e.to_string());
          Self::send_key_response(q, &mut response).await;
          return;
        }

        if q.ctx.this.inner.opts.keyring_file.is_some() {
          if let Err(e) = q.ctx.this.write_keyring_file().await {
            tracing::error!(err=%e, "serf: failed to write keyring file");
            response.message = SmolStr::new(e.to_string());
            Self::send_key_response(q, &mut response).await;
            return;
          }
        }

        response.result = true;
        Self::send_key_response(q, &mut response).await;
      }
      None => {
        tracing::error!(
          err = "encryption enabled but keyring is empty",
          "serf: fail to handle use key"
        );
        response.message = SmolStr::new("encryption enabled but keyring is empty");
        Self::send_key_response(q, &mut response).await;
      }
    }
  }

  #[cfg(feature = "encryption")]
  async fn handle_remove_key(ev: impl AsRef<QueryEvent<T, D>> + Send) {
    let q = ev.as_ref();
    let mut response = KeyResponseMessage::default();

    let req = match crate::types::decode_message::<T::Id, T::ResolvedAddress>(&q.payload) {
      Ok(msg) => match msg {
        MessageRef::KeyRequest(req) => req,
        msg => {
          tracing::error!(err = "unexpected message type", "serf: {}", msg.ty());
          Self::send_key_response(q, &mut response).await;
          return;
        }
      },
      Err(e) => {
        tracing::error!(target="serf", err=%e, "failed to decode key request");
        Self::send_key_response(q, &mut response).await;
        return;
      }
    };

    if !q.ctx.this.encryption_enabled() {
      tracing::error!(
        err = "encryption is not enabled",
        "serf: fail to handle remove key"
      );
      response.message = SmolStr::new("encryption is not enabled");
      Self::send_key_response(q, &mut response).await;
      return;
    }

    tracing::info!("serf: received remove-key query");
    let kr = q.ctx.this.inner.memberlist.keyring();
    match kr {
      Some(kr) => {
        if let Err(e) = kr.remove(&req.key.unwrap()) {
          tracing::error!(err=%e, "serf: failed to remove key");
          response.message = SmolStr::new(e.to_string());
          Self::send_key_response(q, &mut response).await;
          return;
        }

        if q.ctx.this.inner.opts.keyring_file.is_some() {
          if let Err(e) = q.ctx.this.write_keyring_file().await {
            tracing::error!(err=%e, "serf: failed to write keyring file");
            response.message = SmolStr::new(e.to_string());
            Self::send_key_response(q, &mut response).await;
            return;
          }
        }

        response.result = true;
        Self::send_key_response(q, &mut response).await;
      }
      None => {
        tracing::error!(
          err = "encryption enabled but keyring is empty",
          "serf: fail to handle remove key"
        );
        response.message = SmolStr::new("encryption enabled but keyring is empty");
        Self::send_key_response(q, &mut response).await;
      }
    }
  }

  /// Invoked when a query is received to return a list of all
  /// installed keys the Serf instance knows of.
  #[cfg(feature = "encryption")]
  async fn handle_list_keys(ev: impl AsRef<QueryEvent<T, D>> + Send) {
    let q = ev.as_ref();
    let mut response = KeyResponseMessage::default();
    if !q.ctx.this.encryption_enabled() {
      tracing::error!(
        err = "encryption is not enabled",
        "serf: fail to handle list keys"
      );
      response.message = SmolStr::new("encryption is not enabled");
      Self::send_key_response(q, &mut response).await;
      return;
    }

    tracing::info!("serf: received list-keys query");
    let kr = q.ctx.this.inner.memberlist.keyring();
    match kr {
      Some(kr) => {
        for k in kr.keys() {
          response.keys.push(k);
        }

        let primary_key = kr.primary_key();
        response.primary_key = Some(primary_key);
        response.result = true;
        Self::send_key_response(q, &mut response).await;
      }
      None => {
        tracing::error!(
          err = "encryption enabled but keyring is empty",
          "serf: fail to handle list keys"
        );
        response.message = SmolStr::new("encryption enabled but keyring is empty");
        Self::send_key_response(q, &mut response).await;
      }
    }
  }

  #[cfg(feature = "encryption")]
  pub(crate) fn key_list_response_with_correct_size(
    q: &QueryEvent<T, D>,
    resp: &mut KeyResponseMessage,
  ) -> Result<
    (
      Bytes,
      crate::types::QueryResponseMessage<T::Id, T::ResolvedAddress>,
    ),
    Error<T, D>,
  > {
    let actual = resp.keys.len();

    // if the provided list of keys is smaller then the max allowed, just iterate over it
    // to avoid an out of bound access when truncating
    let max_list_keys =
      (q.ctx.this.inner.opts.query_response_size_limit / MIN_ENCODED_KEY_LENGTH).min(actual);

    for i in (0..=max_list_keys).rev() {
      let kraw = crate::types::encode_message_to_bytes(&*resp)?;

      // create response
      let qresp = q.create_response(kraw.clone());

      let encoded_len = crate::types::encoded_message_len(&qresp);
      // Check the size limit
      if q.check_response_size(encoded_len).is_err() {
        resp.keys.drain(i..);
        resp.message = SmolStr::new(format!(
          "truncated key list response, showing first {} of {} keys",
          i, actual
        ));
        continue;
      }

      // encode response
      let qraw = crate::types::encode_message_to_bytes(&qresp)?;

      if actual > i {
        tracing::warn!("serf: {}", resp.message);
      }
      return Ok((qraw, qresp));
    }
    Err(Error::fail_truncate_response())
  }

  #[cfg(feature = "encryption")]
  async fn send_key_response(q: &QueryEvent<T, D>, resp: &mut KeyResponseMessage) {
    match q.name.as_str() {
      "_serf_list_keys" => {
        let (raw, qresp) = match Self::key_list_response_with_correct_size(q, resp) {
          Ok((raw, qresp)) => (raw, qresp),
          Err(e) => {
            tracing::error!(target="serf", err=%e);
            return;
          }
        };

        if let Err(e) = q.respond_with_message_and_response(raw, qresp).await {
          tracing::error!(target="serf", err=%e, "failed to respond to key query");
        }
      }
      _ => match crate::types::encode_message_to_bytes(&*resp) {
        Ok(raw) => {
          if let Err(e) = q.respond(raw).await {
            tracing::error!(target="serf", err=%e, "failed to respond to key query");
          }
        }
        Err(e) => {
          tracing::error!(target="serf", err=%e, "failed to encode key response");
        }
      },
    }
  }
}
