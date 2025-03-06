use std::time::Duration;

use super::Agent;

const MDNS_POLL_INTERVAL: Duration = Duration::from_secs(60);
const MDNS_QUIET_INTERVAL: Duration = Duration::from_millis(100);

/// Used to advertise ourself using mDNS and to
/// attempt to join peers periodically using mDNS queries.
pub struct AgentMDNS<I, A> {}
