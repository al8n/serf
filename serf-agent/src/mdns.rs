use std::{collections::HashSet, net::{IpAddr, Ipv4Addr, SocketAddr}, time::Duration};

use agnostic::{time::AsyncInterval, Runtime, RuntimeLite};
use futures::{FutureExt, StreamExt};
use serf_core::{delegate::Delegate, transport::Transport, types::{MaybeResolvedAddress, Node, SmallVec}};
use smol_str::{format_smolstr, SmolStr, ToSmolStr};
use agnostic_mdns::{worksteal::{channel::{bounded, Sender}, query, Server, ServiceEntry}, QueryParam, ServerOptions, Service, ServiceBuilder};

use crate::options::MDNSOptions;

use super::Agent;

const MDNS_POLL_INTERVAL: Duration = Duration::from_secs(60);
const MDNS_QUIET_INTERVAL: Duration = Duration::from_millis(100);

/// Represents a transport protocol used in mDNS (multicast DNS) service advertisements.
///
/// This trait defines how a transport protocol identifies itself in mDNS service records.
/// In mDNS, transport protocols are represented as strings prefixed with an underscore
/// (e.g., "_tcp", "_udp") and are used in service type definitions.
///
/// # Examples
///
/// ```
/// struct TCP;
///
/// impl MDNSProtocol for TCP {
///     fn protocol_identifier() -> &'static str {
///         "_tcp"
///     }
/// }
/// ```
pub trait MDNSProtocol {
  /// Returns the standardized string identifier for this mDNS transport protocol.
  ///
  /// The returned string should:
  /// - Begin with an underscore
  /// - Be lowercase
  /// - Correspond to a recognized protocol (e.g., "_tcp", "_udp")
  ///
  /// This identifier is used when constructing the full service type string
  /// in mDNS advertisements.
  fn protocol_identifier() -> &'static str;
}

/// Used to advertise ourself using mDNS and to
/// attempt to join peers periodically using mDNS queries.
pub struct AgentMDNS<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
  T::Runtime: Runtime,
{
  seen: HashSet<SocketAddr>,
  agent: Agent<T, D>,
  server: Server<<T::Runtime as Runtime>::Net, Service>,
  replay: bool,
  discover: SmolStr,
  ipv4_interface: Option<Ipv4Addr>,
  ipv6_interface: Option<u32>,
  disable_ipv4: bool,
  disable_ipv6: bool,
}

impl<T, D> AgentMDNS<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport + MDNSProtocol,
  T::Runtime: Runtime,
  T::Id: From<SocketAddr>,
  T::ResolvedAddress: From<SocketAddr>,
{
  /// Used to create a new AgentMDNS
  pub async fn new(
    agent: Agent<T, D>,
    replay: bool,
    node: SmolStr,
    discover: SmolStr,
    config: MDNSOptions,
    bind: SocketAddr,
  ) -> std::io::Result<Self> {
    // Create the service
    let service = mdns_name::<T>(&discover);
    let service = ServiceBuilder::new(node.as_str().into(), service.as_str().into())
      .with_port(bind.port())
      .with_ip(bind.ip())
      .with_txt_record(format_smolstr!("Serf '{}' cluster", discover))
      .finalize()?;

    let disable_ipv4 = config.disable_ipv4();
    let disable_ipv6 = config.disable_ipv6();

    let (v4_iface, v6_iface) = match (disable_ipv4, disable_ipv6) {
      (true, true) => {
        (None, None)
      },
      (true, false) => {
        (None, getifs::ifname_to_v6_iface(config.interface())?)
      }
      (false, true) => {
        (getifs::ifname_to_v4_iface(config.interface())?, None)
      }
      (false, false) => {
        getifs::ifname_to_iface(config.interface())?
      }
    };

    // configure mDNS server
    let mut opts = ServerOptions::new();

    if let Some(v4_iface) = v4_iface {
      opts = opts.with_ipv4_interface(v4_iface);
    }

    if let Some(v6_iface) = v6_iface {
      opts = opts.with_ipv6_interface(v6_iface);
    }

    let server = Server::new(service, opts).await?;

    Ok(Self {
      seen: HashSet::new(),
      agent,
      server,
      replay,
      discover,
      ipv4_interface: v4_iface,
      ipv6_interface: v6_iface,
      disable_ipv4,
      disable_ipv6,
    })
  }

  /// A long running task that scans for new hosts periodically
  async fn run(mut self) {
    let (mut tx, mut rx) = bounded(32);
    
    self.poll(tx.clone());

    let interval = <T::Runtime as RuntimeLite>::interval(MDNS_POLL_INTERVAL);
    let quiet = <T::Runtime as RuntimeLite>::interval(Duration::MAX);
    futures::pin_mut!(interval, quiet);

    let mut join = SmallVec::new();
    let shutdown_rx = self.agent.shutdown_rx();

    loop {
      futures::select! {
        _ = shutdown_rx.recv().fuse() => {
          break;
        }
        h = rx.recv().fuse() => {
          if let Ok(h) = h {
            // Format the host address
            let ip = h.ipv4_addr().copied().map(IpAddr::from).or_else(|| h.ipv6_addr().copied().map(IpAddr::from));
            let Some(ip) = ip else {
              tracing::warn!(name=%h.name(), "agent.mdns: failed to format address, service entry is missing an IP address");
              continue;
            };

            let addr = SocketAddr::new(ip, h.port());

            // Skip if we've already seen this host
            if self.seen.contains(&addr) {
              continue;
            }

            // Queue for handling
            join.push(addr);
            quiet.reset(MDNS_QUIET_INTERVAL);
          } else {
            let (ntx, nrx) = bounded(32);
            tx = ntx;
            rx = nrx;
          }
        }
        _ = quiet.next().fuse() => {
          if join.is_empty() {
            continue;
          }

          // Attempt the join
          let res = self.agent.join(join.iter().map(|&addr| {
            Node::<T::Id, _>::new(<T::Id as From<_>>::from(addr), MaybeResolvedAddress::Resolved(<T::ResolvedAddress as From<_>>::from(addr)))
          }), self.replay).await;

          match res {
            Ok(joins) => {
              tracing::info!(joins=%joins.len(), "agent.mdns: joined new hosts");
            },
            Err((joined, e)) => {
              if !joined.is_empty() {
                tracing::info!(joins=%joined.len(), "agent.mdns: joined new hosts");
              }
              tracing::error!(err=%e, "agent.mdns: failed to join");
            }
          }

          // Mark all as seen
          for addr in join.drain(..) {
            self.seen.insert(addr);
          }
        }
        _ = interval.next().fuse() => {
          self.poll(tx.clone());
        }
      }
    }
  }

  /// Invoked periodically to check for new hosts
  fn poll(&self, hosts: Sender<ServiceEntry>) {
    let poll = Poll {
      discover: self.discover.clone(),
      ipv4_interface: self.ipv4_interface,
      ipv6_interface: self.ipv6_interface,
      disable_ipv4: self.disable_ipv4,
      disable_ipv6: self.disable_ipv6,
    };

    <T::Runtime as RuntimeLite>::spawn_detach(poll.run::<T>(hosts));
  }
}

struct Poll {
  discover: SmolStr,
  ipv4_interface: Option<Ipv4Addr>,
  ipv6_interface: Option<u32>,
  disable_ipv4: bool,
  disable_ipv6: bool,
}

impl Poll {
  async fn run<T>(self, tx: Sender<ServiceEntry>,)
  where
    T: Transport + MDNSProtocol,
    T::Runtime: Runtime,
  {
    let srv = mdns_name::<T>(&self.discover);
    let mut params = QueryParam::new(srv.as_str().into())
      .with_disable_ipv4(self.disable_ipv4)
      .with_disable_ipv6(self.disable_ipv6);

    if let Some(ipv4) = self.ipv4_interface {
      params = params.with_ipv4_interface(ipv4);
    }

    if let Some(ipv6) = self.ipv6_interface {
      params = params.with_ipv6_interface(ipv6);
    }

    if let Err(e) = query::<<T::Runtime as Runtime>::Net>(params, tx).await {
      tracing::error!(err=%e, "agent.mdns: failed to poll for new hosts");
    }
  }
}


fn mdns_name<P: MDNSProtocol>(discover: &str) -> SmolStr {
  format_smolstr!("_serf_{}.{}", discover, P::protocol_identifier())
}
