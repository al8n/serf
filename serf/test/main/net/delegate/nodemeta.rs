macro_rules! test_mod {
  ($rt:ident) => {
    paste::paste! {
      mod [< $rt:snake >] {
        use crate::[< $rt:snake _run >];
        use serf::{
          net::{
            resolver::socket_addr::SocketAddrResolver, stream_layer::tcp::Tcp, NetTransport,
            NetTransportOptions,
          },
          [< $rt:snake >]::[< $rt:camel Runtime >],

        };
        use serf_core::tests::{delegate::{delegate_nodemeta, delegate_nodemeta_panic}, next_socket_addr_v4, next_socket_addr_v6};
        use smol_str::SmolStr;

        #[test]
        fn test_delegate_nodemeta_v4() {
          let name = "delegate_nodemeta_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));
          [< $rt:snake _run >](delegate_nodemeta::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
          >(opts));
        }

        #[test]
        fn test_delegate_nodemeta_v6() {
          let name = "delegate_nodemeta_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());
          [< $rt:snake _run >](delegate_nodemeta::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
          >(opts));
        }

        #[test]
        #[should_panic]
        fn test_delegate_nodemeta_panic_v4() {
          let name = "delegate_nodemeta_panic_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));
          [< $rt:snake _run >](delegate_nodemeta_panic::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
          >(opts));
        }

        #[test]
        #[should_panic]
        fn test_delegate_nodemeta_panic_v6() {
          let name = "delegate_nodemeta_panic_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());
          [< $rt:snake _run >](delegate_nodemeta_panic::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

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

#[cfg(feature = "smol")]
test_mod!(smol);
