use std::{
  collections::{HashMap, HashSet},
  hash::Hash,
};

use super::Filter;
use arbitrary::{Arbitrary, Unstructured};
use indexmap::{IndexMap, IndexSet};
use memberlist_proto::TinyVec;

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
      Filter::Tag {
        tag: u.arbitrary()?,
        expr: u.arbitrary()?,
      }
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
