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
        use serf_core::tests::{event::serf_events_leave_avoid_infinite_rebroadcast, next_socket_addr_v4, next_socket_addr_v6};
        use smol_str::SmolStr;

        #[test]
        fn test_serf_events_leave_avoid_infinite_rebroadcast_v4() {
          let name = "serf_events_leave_avoid_infinite_rebroadcast1_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_events_leave_avoid_infinite_rebroadcast2_v4";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_events_leave_avoid_infinite_rebroadcast3_v4";
          let mut opts3 = NetTransportOptions::new(SmolStr::new(name));
          opts3.add_bind_address(next_socket_addr_v4(0));

          let name = "serf_events_leave_avoid_infinite_rebroadcast4_v4";
          let mut opts4 = NetTransportOptions::new(SmolStr::new(name));
          opts4.add_bind_address(next_socket_addr_v4(0));

          [< $rt:snake _run >](serf_events_leave_avoid_infinite_rebroadcast::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
            _,
          >(opts, opts2, opts3, opts4, |id, addr| async move {
            let mut opts2 = NetTransportOptions::new(id);
            opts2.add_bind_address(addr);
            opts2
          }));
        }

        #[test]
        fn test_serf_events_leave_avoid_infinite_rebroadcast_v6() {
          let name = "serf_events_leave_avoid_infinite_rebroadcast1_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());

          let name = "serf_events_leave_avoid_infinite_rebroadcast2_v6";
          let mut opts2 = NetTransportOptions::new(SmolStr::new(name));
          opts2.add_bind_address(next_socket_addr_v6());

          let name = "serf_events_leave_avoid_infinite_rebroadcast3_v6";
          let mut opts3 = NetTransportOptions::new(SmolStr::new(name));
          opts3.add_bind_address(next_socket_addr_v6());

          let name = "serf_events_leave_avoid_infinite_rebroadcast4_v6";
          let mut opts4 = NetTransportOptions::new(SmolStr::new(name));
          opts4.add_bind_address(next_socket_addr_v6());

          [< $rt:snake _run >](serf_events_leave_avoid_infinite_rebroadcast::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
            _,
          >(opts, opts2, opts3, opts4, |id, addr| async move {
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

#[cfg(feature = "async-std")]
test_mod!(async_std);

#[cfg(feature = "smol")]
test_mod!(smol);
