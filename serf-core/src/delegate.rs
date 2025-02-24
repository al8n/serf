use memberlist_core::{CheapClone, transport::Id};

mod merge;
pub use merge::*;

mod reconnect;
pub use reconnect::*;

mod transform;
pub use transform::*;

mod composite;
pub use composite::*;

/// [`Delegate`] is the trait that clients must implement if they want to hook
/// into the gossip layer of [`Serf`](crate::Serf). All the methods must be thread-safe,
/// as they can and generally will be called concurrently.
pub trait Delegate:
  MergeDelegate<Id = <Self as Delegate>::Id, Address = <Self as Delegate>::Address>
  + TransformDelegate<Id = <Self as Delegate>::Id, Address = <Self as Delegate>::Address>
  + ReconnectDelegate<Id = <Self as Delegate>::Id, Address = <Self as Delegate>::Address>
{
  /// The id type of the delegate
  type Id: Id;
  /// The address type of the delegate
  type Address: CheapClone + Send + Sync + 'static;
}
