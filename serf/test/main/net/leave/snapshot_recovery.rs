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
        use serf_core::tests::{leave::serf_leave_snapshot_recovery, next_socket_addr_v4, next_socket_addr_v6};
        use smol_str::SmolStr;

        #[test]
        fn test_serf_leave_snapshot_recovery_v4() {
          let name = "serf_leave_snapshot_recovery1_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_leave_snapshot_recovery2_v4";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v4(0));

          [< $rt:snake _run >](serf_leave_snapshot_recovery::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
            _
          >(opts, opts2, |id, addr| async move {
            let mut opts2 = NetTransportOptions::new(id);
            opts2.add_bind_address(addr);
            opts2
          }));
        }

        #[test]
        fn test_serf_leave_snapshot_recovery_v6() {
          let name = "serf_leave_snapshot_recovery1_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());

          let name = "serf_leave_snapshot_recovery2_v6";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v6());

          [< $rt:snake _run >](serf_leave_snapshot_recovery::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
            _
          >(opts, opts2, |id, addr| async move {
            let mut opts2 = NetTransportOptions::new(id);
            opts2.add_bind_address(addr);
            opts2
          }));
        }
      }
    }
  };
}

#[cfg(feature = "tokio")]
test_mod!(tokio);

#[cfg(feature = "smol")]
test_mod!(smol);
