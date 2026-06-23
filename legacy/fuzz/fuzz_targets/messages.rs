#![no_main]
#![allow(clippy::type_complexity)]

use libfuzzer_sys::fuzz_target;

use serf_core::types::{
  Node,
  fuzzy::{Message, encodable_round_trip},
};

fuzz_target!(
  |data: (Message<String, Vec<u8>>, Option<Node<String, Vec<u8>>>)| {
    assert!(encodable_round_trip(data.0, data.1));
  }
);
