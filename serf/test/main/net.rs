#[path = "./net/delegate.rs"]
mod delegate;

#[path = "./net/event.rs"]
mod event;

#[path = "./net/leave.rs"]
mod leave;

#[path = "./net/reap.rs"]
mod reap;

#[path = "./net/reconnect.rs"]
mod reconnect;

#[path = "./net/remove.rs"]
mod remove;

#[path = "./net/snapshot.rs"]
mod snapshot;

#[path = "./net/update.rs"]
mod update;

#[path = "./net/role.rs"]
mod role;

#[path = "./net/update_tags.rs"]
mod update_tags;

#[path = "./net/get_queue_max.rs"]
mod get_queue_max;

#[path = "./net/local_member.rs"]
mod local_member;

#[path = "./net/num_nodes.rs"]
mod num_nodes;

#[path = "./net/state.rs"]
mod state;

#[path = "./net/stats.rs"]
mod stats;

#[path = "./net/coordinates.rs"]
mod coordinates;

#[path = "./net/name_resolution.rs"]
mod name_resolution;

#[path = "./net/join.rs"]
mod join;

#[cfg(feature = "encryption")]
#[path = "./net/write_keyring_file.rs"]
mod write_keyring_file;
