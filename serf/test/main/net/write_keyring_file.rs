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
        use serf_core::{tests::{serf_write_keyring_file, next_socket_addr_v4, next_socket_addr_v6}, MemberlistOptions, types::EncryptionAlgorithm};
        use smol_str::SmolStr;

        #[test]
        fn test_serf_write_keyring_file_v4() {
          let name = "serf_write_keyring_file_v4";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v4(0));

          [< $rt:snake _run >](serf_write_keyring_file::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
          >(|kr| {
            (opts, MemberlistOptions::lan().with_primary_key(kr).with_gossip_verify_outgoing(true).with_encryption_algo(EncryptionAlgorithm::default()))
          }));
        }

        #[test]
        fn test_serf_write_keyring_file_v6() {
          let name = "serf_write_keyring_file_v6";
          let mut opts = NetTransportOptions::new(SmolStr::new(name));
          opts.add_bind_address(next_socket_addr_v6());

          [< $rt:snake _run >](serf_write_keyring_file::<
            NetTransport<
              SmolStr,
              SocketAddrResolver<[< $rt:camel Runtime >]>,
              Tcp<[< $rt:camel Runtime >]>,

              [< $rt:camel Runtime >],
            >,
          >(|kr| {
            (opts, MemberlistOptions::lan().with_primary_key(kr).with_gossip_verify_outgoing(true).with_encryption_algo(EncryptionAlgorithm::default()))
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
