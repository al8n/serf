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
        use serf_core::tests::{leave::serf_force_leave_leaving, next_socket_addr_v4, next_socket_addr_v6};
        use smol_str::SmolStr;

        #[test]
        fn test_serf_force_leave_leaving_v4() {
          let name = "serf_force_leave_leaving1_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_force_leave_leaving2_v4";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_force_leave_leaving3_v4";
          let mut opts3 = NetTransportOptions::new(SmolStr::new(name));
          opts3.add_bind_address(next_socket_addr_v4(0));

          [< $rt:snake _run >](serf_force_leave_leaving::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
          >(opts, opts2, opts3));
        }

        #[test]
        fn test_serf_force_leave_leaving_v6() {
          let name = "serf_force_leave_leaving1_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());

          let name = "serf_force_leave_leaving2_v6";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v6());

          let name = "serf_force_leave_leaving3_v6";
          let mut opts3 = NetTransportOptions::new(SmolStr::new(name));
          opts3.add_bind_address(next_socket_addr_v6());

          [< $rt:snake _run >](serf_force_leave_leaving::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

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
