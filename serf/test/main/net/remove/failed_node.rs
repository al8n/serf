macro_rules! test_mod {
  ($rt:ident) => {
    paste::paste! {
      mod [< $rt:snake >] {
        use std::net::SocketAddr;

        use crate::[< $rt:snake _run >];
        use serf::{
          net::{
            resolver::socket_addr::SocketAddrResolver, stream_layer::tcp::Tcp, NetTransport,
            NetTransportOptions,
          },
          [< $rt:snake >]::[< $rt:camel Runtime >],
          transport::Lpe,
        };
        use serf_core::tests::{remove::serf_remove_failed_node, next_socket_addr_v4, next_socket_addr_v6};
        use smol_str::SmolStr;

        #[test]
        fn test_serf_remove_failed_node_v4() {
          let name = "serf_remove_failed_node1_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_remove_failed_node2_v4";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_remove_failed_node3_v4";
          let mut opts3 = NetTransportOptions::new(SmolStr::new(name));
          opts3.add_bind_address(next_socket_addr_v4(0));

          [< $rt:snake _run >](serf_remove_failed_node::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,
              Lpe<SmolStr, SocketAddr>,
              [< $rt:camel Runtime >],
            >,
          >(opts, opts2, opts3));
        }

        #[test]
        fn test_serf_remove_failed_node_v6() {
          let name = "serf_remove_failed_node1_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());

          let name = "serf_remove_failed_node2_v6";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v6());

          let name = "serf_remove_failed_node3_v6";
          let mut opts3 = NetTransportOptions::new(SmolStr::new(name));
          opts3.add_bind_address(next_socket_addr_v6());

          [< $rt:snake _run >](serf_remove_failed_node::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,
              Lpe<SmolStr, SocketAddr>,
              [< $rt:camel Runtime >],
            >,
          >(opts, opts2, opts3));
        }
      }
    }
  };
}

#[cfg(feature = "tokio")]
test_mod!(tokio);

#[cfg(feature = "async-std")]
test_mod!(async_std);

#[cfg(feature = "smol")]
test_mod!(smol);