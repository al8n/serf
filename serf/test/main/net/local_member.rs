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
        use serf_core::tests::{serf_local_member, next_socket_addr_v4, next_socket_addr_v6};
        use smol_str::SmolStr;

        #[test]
        fn test_serf_local_member_v4() {
          let name = "serf_local_member_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));

          [< $rt:snake _run >](serf_local_member::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,
              Lpe<SmolStr, SocketAddr>,
              [< $rt:camel Runtime >],
            >,
          >(opts));
        }

        #[test]
        fn test_serf_local_member_v6() {
          let name = "serf_local_member_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());

          [< $rt:snake _run >](serf_local_member::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,
              Lpe<SmolStr, SocketAddr>,
              [< $rt:camel Runtime >],
            >,
          >(opts));
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