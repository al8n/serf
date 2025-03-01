use core::hash::Hash;

use quickcheck::{Arbitrary, Gen};
use smol_str::SmolStr;

use super::{
  ConflictResponseMessage, DelegateVersion, Filter, JoinMessage, LamportTime, LeaveMessage, Member,
  MemberStatus, MessageType, ProtocolVersion, PushPullMessage, QueryFlag, QueryMessage,
  QueryResponseMessage, TagFilter, Tags, UserEvent, UserEventMessage, UserEvents, coordinate::Coordinate,
};

#[cfg(feature = "encryption")]
use super::{KeyRequestMessage, KeyResponseMessage};

impl Arbitrary for ProtocolVersion {
  fn arbitrary(g: &mut Gen) -> Self {
    ProtocolVersion::from(u8::arbitrary(g))
  }
}

impl Arbitrary for DelegateVersion {
  fn arbitrary(g: &mut Gen) -> Self {
    DelegateVersion::from(u8::arbitrary(g))
  }
}

impl Arbitrary for UserEvent {
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      name: String::arbitrary(g).into(),
      payload: Vec::arbitrary(g).into(),
    }
  }
}

impl Arbitrary for UserEvents {
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      ltime: u64::arbitrary(g).into(),
      events: Vec::arbitrary(g).into(),
    }
  }
}

impl Arbitrary for UserEventMessage {
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      ltime: u64::arbitrary(g).into(),
      name: String::arbitrary(g).into(),
      payload: Vec::arbitrary(g).into(),
      cc: bool::arbitrary(g),
    }
  }
}

impl Arbitrary for LamportTime {
  fn arbitrary(g: &mut Gen) -> Self {
    LamportTime::from(u64::arbitrary(g))
  }
}

impl Arbitrary for TagFilter {
  fn arbitrary(g: &mut Gen) -> Self {
    Self::new()
      .with_tag(String::arbitrary(g).into())
      .maybe_expr(if Arbitrary::arbitrary(g) {
        let complexity = *g.choose(&[1, 2, 3, 4, 5]).unwrap();
        let mut patterns = Vec::new();

        // Basic character classes and quantifiers
        let character_classes = vec![
          r"\d",
          r"\w",
          r"\s",
          r"[a-z]",
          r"[A-Z]",
          r"[0-9]",
          r"[a-zA-Z]",
          r"[a-zA-Z0-9]",
          r".",
        ];

        let quantifiers = vec!["", "*", "+", "?", "{1,3}", "{2,5}"];

        // Add more complex patterns for higher complexity
        let mut extended_classes = character_classes.clone();
        if complexity > 1 {
          extended_classes.extend(vec![r"[^a-z]", r"[^0-9]", r"\D", r"\W", r"\S"]);
        }

        if complexity > 2 {
          // Add a group with random content
          let char_class = *g.choose(&extended_classes).unwrap();
          let quantifier = *g.choose(&quantifiers).unwrap();
          patterns.push(format!("({}{})", char_class, quantifier));
        }

        // Generate random pattern parts
        for _ in 0..complexity {
          let char_class = *g.choose(&extended_classes).unwrap();
          let quantifier = *g.choose(&quantifiers).unwrap();
          patterns.push(format!("{}{}", char_class, quantifier));
        }

        // Maybe add anchors for higher complexity
        if complexity > 2 && rand::random_ratio(7, 10) {
          if Arbitrary::arbitrary(g) {
            patterns.insert(0, "^".to_string());
          }
          if Arbitrary::arbitrary(g) {
            patterns.push("$".to_string());
          }
        }

        // Add alternation for even higher complexity
        if complexity > 3 && rand::random_ratio(6, 10) {
          let char_class = *g.choose(&extended_classes).unwrap();
          let quantifier = *g.choose(&quantifiers).unwrap();
          patterns.push(format!("|{}{}", char_class, quantifier));
        }

        Some(patterns.join("").try_into().unwrap())
      } else {
        None
      })
  }
}

impl Arbitrary for Tags {
  fn arbitrary(g: &mut Gen) -> Self {
    Self::from_iter(
      Vec::<(String, String)>::arbitrary(g)
        .into_iter()
        .map(|(k, v)| (SmolStr::from(k), SmolStr::from(v))),
    )
  }
}

impl<I, A> Arbitrary for QueryMessage<I, A>
where
  I: Arbitrary,
  A: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      ltime: Arbitrary::arbitrary(g),
      flags: Arbitrary::arbitrary(g),
      id: Arbitrary::arbitrary(g),
      from: Arbitrary::arbitrary(g),
      filters: Vec::arbitrary(g).into(),
      relay_factor: Arbitrary::arbitrary(g),
      timeout: Arbitrary::arbitrary(g),
      name: String::arbitrary(g).into(),
      payload: Vec::arbitrary(g).into(),
    }
  }
}

impl Arbitrary for QueryFlag {
  fn arbitrary(g: &mut Gen) -> Self {
    if bool::arbitrary(g) {
      QueryFlag::NO_BROADCAST
    } else {
      QueryFlag::ACK
    }
  }
}

impl<I, A> Arbitrary for QueryResponseMessage<I, A>
where
  I: Arbitrary,
  A: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      ltime: Arbitrary::arbitrary(g),
      id: Arbitrary::arbitrary(g),
      from: Arbitrary::arbitrary(g),
      flags: Arbitrary::arbitrary(g),
      payload: Vec::arbitrary(g).into(),
    }
  }
}

impl<I> Arbitrary for Filter<I>
where
  I: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    if bool::arbitrary(g) {
      Filter::Id(Vec::<I>::arbitrary(g).into())
    } else {
      Filter::Tag(TagFilter::arbitrary(g))
    }
  }
}

impl<I> Arbitrary for PushPullMessage<I>
where
  I: Arbitrary + Hash + Eq,
{
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      ltime: Arbitrary::arbitrary(g),
      status_ltimes: Vec::<(I, LamportTime)>::arbitrary(g).into_iter().collect(),
      left_members: Vec::<I>::arbitrary(g).into_iter().collect(),
      event_ltime: Arbitrary::arbitrary(g),
      events: Vec::<UserEvents>::arbitrary(g).into(),
      query_ltime: Arbitrary::arbitrary(g),
    }
  }
}

impl Arbitrary for MemberStatus {
  fn arbitrary(g: &mut Gen) -> Self {
    MemberStatus::from(u8::arbitrary(g))
  }
}

impl<I, A> Arbitrary for Member<I, A>
where
  I: Arbitrary,
  A: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      node: Arbitrary::arbitrary(g),
      tags: Tags::arbitrary(g).into(),
      status: Arbitrary::arbitrary(g),
      memberlist_protocol_version: Arbitrary::arbitrary(g),
      memberlist_delegate_version: Arbitrary::arbitrary(g),
      protocol_version: Arbitrary::arbitrary(g),
      delegate_version: Arbitrary::arbitrary(g),
    }
  }
}

impl<I> Arbitrary for LeaveMessage<I>
where
  I: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      ltime: Arbitrary::arbitrary(g),
      prune: bool::arbitrary(g),
      id: Arbitrary::arbitrary(g),
    }
  }
}

#[cfg(feature = "encryption")]
impl Arbitrary for KeyRequestMessage {
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      key: Arbitrary::arbitrary(g),
    }
  }
}

#[cfg(feature = "encryption")]
impl Arbitrary for KeyResponseMessage {
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      result: Arbitrary::arbitrary(g),
      message: String::arbitrary(g).into(),
      keys: Vec::arbitrary(g).into(),
      primary_key: Arbitrary::arbitrary(g),
    }
  }
}

impl<I> Arbitrary for JoinMessage<I>
where
  I: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      ltime: Arbitrary::arbitrary(g),
      id: Arbitrary::arbitrary(g),
    }
  }
}

impl<I, A> Arbitrary for ConflictResponseMessage<I, A>
where
  I: Arbitrary,
  A: Arbitrary,
{
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      member: Arbitrary::arbitrary(g),
    }
  }
}

impl Arbitrary for MessageType {
  fn arbitrary(g: &mut Gen) -> Self {
    MessageType::from(u8::arbitrary(g) % Self::ALL.len() as u8)
  }
}

impl Arbitrary for Coordinate {
  fn arbitrary(g: &mut Gen) -> Self {
    Self {
      portion: Vec::arbitrary(g).into(),
      error: Arbitrary::arbitrary(g),
      adjustment: Arbitrary::arbitrary(g),
      height: Arbitrary::arbitrary(g),
    }
  }
}
