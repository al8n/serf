use std::{
  collections::{HashMap, HashSet},
  hash::Hash,
};

use super::{Filter, MessageType, TagFilter};
use arbitrary::{Arbitrary, Unstructured};
use indexmap::{IndexMap, IndexSet};
use memberlist_core::proto::TinyVec;

pub(super) fn into<'a, F, T>(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<T>
where
  F: arbitrary::Arbitrary<'a>,
  T: From<F>,
{
  u.arbitrary::<F>().map(Into::into)
}

pub(super) fn arbitrary_indexmap<'a, K, V>(
  u: &mut arbitrary::Unstructured<'a>,
) -> arbitrary::Result<IndexMap<K, V>>
where
  K: Arbitrary<'a> + Hash + Eq,
  V: Arbitrary<'a>,
{
  let map = u.arbitrary::<HashMap<K, V>>()?;
  Ok(IndexMap::from_iter(map))
}

pub(super) fn arbitrary_indexset<'a, K>(
  u: &mut arbitrary::Unstructured<'a>,
) -> arbitrary::Result<IndexSet<K>>
where
  K: Arbitrary<'a> + Hash + Eq,
{
  let map = u.arbitrary::<HashSet<K>>()?;
  Ok(IndexSet::from_iter(map))
}

impl<'a, I> Arbitrary<'a> for Filter<I>
where
  I: Arbitrary<'a>,
{
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    let kind = u.arbitrary::<bool>()?;
    Ok(if kind {
      Filter::Id(into::<Vec<I>, TinyVec<_>>(u)?)
    } else {
      Filter::Tag(
        TagFilter::new()
          .with_tag(u.arbitrary()?)
          .maybe_expr(if u.arbitrary()? {
            let complexity = u.int_in_range(1..=5)?;
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
              let char_class = u.choose(&extended_classes)?;
              let quantifier = u.choose(&quantifiers)?;
              patterns.push(format!("({}{})", char_class, quantifier));
            }

            // Generate random pattern parts
            for _ in 0..complexity {
              let char_class = u.choose(&extended_classes)?;
              let quantifier = u.choose(&quantifiers)?;
              patterns.push(format!("{}{}", char_class, quantifier));
            }

            // Maybe add anchors for higher complexity
            if complexity > 2 && u.ratio(7, 10)? {
              if u.arbitrary()? {
                patterns.insert(0, "^".to_string());
              }
              if u.arbitrary()? {
                patterns.push("$".to_string());
              }
            }

            // Add alternation for even higher complexity
            if complexity > 3 && u.ratio(6, 10)? {
              let char_class = u.choose(&extended_classes)?;
              let quantifier = u.choose(&quantifiers)?;
              patterns.push(format!("|{}{}", char_class, quantifier));
            }

            Some(patterns.join("").try_into().unwrap())
          } else {
            None
          }),
      )
    })
  }
}

impl<'a> Arbitrary<'a> for super::QueryFlag {
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    Ok(if u.arbitrary()? {
      Self::NO_BROADCAST
    } else {
      Self::ACK
    })
  }
}

impl<'a> Arbitrary<'a> for super::ProtocolVersion {
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    u.arbitrary::<u8>().map(Into::into)
  }
}

impl<'a> Arbitrary<'a> for super::DelegateVersion {
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    u.arbitrary::<u8>().map(Into::into)
  }
}

impl<'a> Arbitrary<'a> for super::MemberStatus {
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    u.arbitrary::<u8>().map(Into::into)
  }
}

#[cfg(feature = "encryption")]
impl<'a, I> Arbitrary<'a> for super::KeyResponse<I>
where
  I: Arbitrary<'a> + Eq + std::hash::Hash,
{
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    Ok(Self {
      messages: arbitrary_indexmap(u)?,
      num_nodes: u.arbitrary()?,
      num_resp: u.arbitrary()?,
      num_err: u.arbitrary()?,
      keys: arbitrary_indexmap(u)?,
      primary_keys: arbitrary_indexmap(u)?,
    })
  }
}

impl<'a> Arbitrary<'a> for MessageType {
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    u.arbitrary::<u8>()
      .map(|val| Self::from(val % Self::ALL.len() as u8))
  }
}

impl<'a> Arbitrary<'a> for super::coordinate::Coordinate {
  fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
    Ok(Self {
      portion: Vec::<f64>::arbitrary(u)?
        .into_iter()
        .map(|f| if f.is_nan() { 0.0 } else { f })
        .collect(),
      error: rand_f64_not_nan(u)?,
      adjustment: rand_f64_not_nan(u)?,
      height: rand_f64_not_nan(u)?,
    })
  }
}

fn rand_f64_not_nan(u: &mut Unstructured<'_>) -> arbitrary::Result<f64> {
  loop {
    let f = f64::arbitrary(u)?;
    if !f.is_nan() {
      return Ok(f);
    }
  }
}
