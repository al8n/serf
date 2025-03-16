use agnostic::{
  Runtime,
  process::{Child, ChildStderr, ChildStdin, ChildStdout, Process},
};
use futures::{AsyncRead, AsyncWrite, lock::Mutex};
use serf_core::{
  Serf,
  delegate::Delegate,
  transport::Transport,
  event::{Event, MemberEventType},
};
use smol_str::{SmolStr, ToSmolStr};

use std::{pin::Pin, str::FromStr, sync::Arc};

use super::invoke::invoke_event_script;

/// A handler that does things when events happen
pub trait EventHandler<T, D>: Send + Sync + 'static {
  /// Returns the unique name of this event handler
  fn name(&self) -> &SmolStr;

  /// Called when an event occurs
  fn handle<'a>(
    &'a self,
    event: &'a Event<T, D>,
  ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
  where
    D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
    T: Transport;
}

struct ScriptEventHandlerInner {
  scripts: Arc<[EventScript]>,
  new_scripts: Arc<[EventScript]>,
}

/// Invokes scripts for the events that it receives.
pub struct ScriptEventHandler<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  inner: Mutex<ScriptEventHandlerInner>,
  serf: Serf<T, D>,
}

impl<T, D> EventHandler<T, D> for ScriptEventHandler<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
  T::Runtime: Runtime,
  <<T::Runtime as Runtime>::Process as Process>::Command: Send,
  ChildStdin<<<<T::Runtime as Runtime>::Process as Process>::Child as Child>::Stdin>:
    AsyncWrite + Unpin + Send,
  ChildStdout<<<<T::Runtime as Runtime>::Process as Process>::Child as Child>::Stdout>:
    AsyncRead + Unpin + Send,
  ChildStderr<<<<T::Runtime as Runtime>::Process as Process>::Child as Child>::Stderr>:
    AsyncRead + Unpin + Send,
{
  fn name(&self) -> &SmolStr {
    const NAME: &SmolStr = &SmolStr::new_inline("script");

    NAME
  }

  fn handle<'a>(
    &'a self,
    event: &'a Event<T, D>,
  ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async {
      // Swap in the new scripts if any
      let scripts = {
        let mut inner = self.inner.lock().await;
        if !inner.new_scripts.is_empty() {
          inner.scripts = inner.new_scripts.clone();
          inner.new_scripts = Arc::default();
        }

        inner.scripts.clone()
      };

      let member = self.serf.local_member().await;
      for script in scripts.iter() {
        if !script.invoke(event) {
          continue;
        }

        if let Err(e) =
          invoke_event_script(script.script.clone(), member.clone(), event.clone()).await
        {
          tracing::error!(err=%e, "serf agent: invoking script", );
        }
      }
    })
  }
}

/// The kind of event that is being processed
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventKind {
  /// A user event
  User,
  /// A query event
  Query,
  /// A member event
  Member(MemberEventType),
  /// '*' unspecified event
  Unspecified,
  /// A custom event type.
  Custom(SmolStr),
}

impl core::fmt::Display for EventKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

impl EventKind {
  /// Returns the string representation of the event kind.
  #[inline]
  pub fn as_str(&self) -> &str {
    match self {
      EventKind::User => "user",
      EventKind::Query => "query",
      EventKind::Member(ty) => ty.as_str(),
      EventKind::Unspecified => "*",
      EventKind::Custom(custom) => custom.as_str(),
    }
  }
}

impl FromStr for EventKind {
  type Err = core::convert::Infallible;

  #[inline]
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(match s {
      "user" => EventKind::User,
      "query" => EventKind::Query,
      "member-join" => EventKind::Member(MemberEventType::Join),
      "member-leave" => EventKind::Member(MemberEventType::Leave),
      "member-failed" => EventKind::Member(MemberEventType::Failed),
      "member-update" => EventKind::Member(MemberEventType::Update),
      "member-reap" => EventKind::Member(MemberEventType::Reap),
      "*" => EventKind::Unspecified,
      _ => EventKind::Custom(s.to_smolstr()),
    })
  }
}

impl From<&str> for EventKind {
  #[inline]
  fn from(s: &str) -> Self {
    EventKind::from_str(s).expect("invalid event kind")
  }
}

/// Used to filter which events are processed
#[viewit::viewit(
  vis_all = "",
  getters(vis_all = "pub", style = "ref"),
  setters(vis_all = "pub", prefix = "with")
)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventFilter {
  kind: EventKind,
  name: SmolStr,
}

impl EventFilter {
  /// Tests whether or not this event script should be invoked
  /// for the given Serf event.
  pub fn invoke<T, D>(&self, e: &Event<T, D>) -> bool
  where
    D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
    T: Transport,
  {
    if self.kind == EventKind::Unspecified {
      return true;
    }

    let ty = self.kind.as_str();
    let ety = e.ty().as_str();
    if ty != ety {
      return false;
    }

    if ty == "user" && !self.name.is_empty() {
      if !e.ty().is_user() {
        return false;
      }

      if self.name.ne(e.name()) {
        return false;
      }
    }

    if ty == "query" && !self.name.is_empty() {
      if !e.ty().is_query() {
        return false;
      }

      if self.name.ne(e.name()) {
        return false;
      }
    }

    true
  }

  /// Checks if this is a valid agent event script.
  pub fn valid(&self) -> bool {
    matches!(self.kind, EventKind::Custom(_))
  }
}

/// A single event script that will be executed in the
/// case of an event, and is configured from the command-line or from
/// a configuration file.
#[viewit::viewit(
  vis_all = "pub(super)",
  getters(vis_all = "pub", style = "ref"),
  setters(vis_all = "pub", prefix = "with")
)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventScript {
  filter: EventFilter,
  script: SmolStr,
}

impl core::ops::Deref for EventScript {
  type Target = EventFilter;

  fn deref(&self) -> &Self::Target {
    &self.filter
  }
}

impl core::fmt::Display for EventScript {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    if !self.name.is_empty() {
      write!(
        f,
        "Event '{}:{}' invoking '{}'",
        self.kind.as_str(),
        self.name,
        self.script
      )
    } else {
      write!(
        f,
        "Event '{}' invoking '{}'",
        self.kind.as_str(),
        self.script
      )
    }
  }
}

/// Takes a string in the format of "type=script" and
/// parses it into an EventScript struct, if it can.
pub fn parse_event_script(v: &str) -> impl Iterator<Item = EventScript> + Send + '_ {
  let (filter, script) = match v.split_once('=') {
    Some((f, s)) => (f, s.to_smolstr()),
    None => ("", v.to_smolstr()),
  };

  let filters = parse_event_filter(filter);
  filters.into_iter().map(move |filter| EventScript {
    filter,
    script: script.clone(),
  })
}

/// A string with the event type filters and
/// parses it into a series of [`EventFilter`]s if it can.
pub fn parse_event_filter(v: &str) -> impl Iterator<Item = EventFilter> + Send + '_ {
  let v = if v.is_empty() { "*" } else { v };

  v.split(',').map(|event| {
    let (event, name) = if let Some(name) = event.strip_prefix("user:") {
      (EventKind::User, name.to_smolstr())
    } else if let Some(name) = event.strip_prefix("query:") {
      (EventKind::Query, name.to_smolstr())
    } else if event.eq("*") {
      (EventKind::Unspecified, SmolStr::default())
    } else {
      let knd = EventKind::from_str(event).expect("invalid event kind");
      if let EventKind::Custom(name) = knd {
        (EventKind::Custom(name), SmolStr::default())
      } else {
        (knd, SmolStr::default())
      }
    };

    EventFilter { kind: event, name }
  })
}
