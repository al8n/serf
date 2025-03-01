use memberlist_core::proto::{
  Data, DataRef, DecodeError, EncodeError, WireType,
  utils::{merge, skip, split},
};

use super::*;

/// A conflict message
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, PartialEq, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct ConflictResponseMessage<I, A> {
  /// The member
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the member")),
    setter(attrs(doc = "Sets the member (Builder pattern)"))
  )]
  member: Member<I, A>,
}

/// The borrow type of conflict message
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, PartialEq)]
pub struct ConflictResponseMessageBorrow<'a, I, A> {
  /// The member
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns the member")),
    setter(attrs(doc = "Sets the member (Builder pattern)"))
  )]
  member: &'a Member<I, A>,
}

impl<'a, I, A> ConflictResponseMessageBorrow<'a, I, A> {
  /// Create a new conflict response message
  pub fn new(member: &'a Member<I, A>) -> Self {
    Self { member }
  }
}

impl<'a, I, A> From<&'a ConflictResponseMessage<I, A>> for ConflictResponseMessageBorrow<'a, I, A> {
  fn from(val: &'a ConflictResponseMessage<I, A>) -> Self {
    Self::new(&val.member)
  }
}

impl<'a, I, A> From<&'a Member<I, A>> for ConflictResponseMessageBorrow<'a, I, A> {
  fn from(val: &'a Member<I, A>) -> Self {
    Self::new(val)
  }
}

impl<I, A> ConflictResponseMessageBorrow<'_, I, A>
where
  I: Data,
  A: Data,
{
  pub(super) fn encoded_len_in(&self) -> usize {
    1 + self.member.encoded_len_with_length_delimited()
  }

  pub(super) fn encode_in(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    let mut offset = 0;

    if offset >= buf.len() {
      return Err(EncodeError::insufficient_buffer(
        self.encoded_len_in(),
        buf.len(),
      ));
    }

    buf[offset] = MEMBER_BYTE;
    offset += 1;
    offset += self
      .member
      .encode_length_delimited(&mut buf[offset..])
      .map_err(|e| e.update(self.encoded_len_in(), buf.len()))?;

    #[cfg(debug_assertions)]
    super::debug_assert_write_eq::<Self>(offset, self.encoded_len_in());

    Ok(offset)
  }
}

const MEMBER_TAG: u8 = 1;
const MEMBER_BYTE: u8 = merge(WireType::LengthDelimited, MEMBER_TAG);

/// The reference to a [`ConflictResponseMessage`].
#[viewit::viewit(getters(style = "ref", vis_all = "pub"), setters(skip), vis_all = "")]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConflictResponseMessageRef<'a, I, A> {
  #[viewit(getter(const, style = "ref", attrs(doc = "Returns the member")))]
  member: MemberRef<'a, I, A>,
}

impl<'a, I, A> DataRef<'a, ConflictResponseMessage<I, A>>
  for ConflictResponseMessageRef<'a, I::Ref<'a>, A::Ref<'a>>
where
  I: Data,
  A: Data,
{
  fn decode(buf: &'a [u8]) -> Result<(usize, Self), DecodeError>
  where
    Self: Sized,
  {
    let mut offset = 0;
    let mut member = None;

    while offset < buf.len() {
      match buf[offset] {
        MEMBER_BYTE => {
          if member.is_some() {
            return Err(DecodeError::duplicate_field(
              "ConflictResponseMessage",
              "member",
              MEMBER_TAG,
            ));
          }
          offset += 1;

          let (len, val) = <MemberRef<'_, I::Ref<'_>, A::Ref<'_>> as DataRef<'_, Member<I, A>>>::decode_length_delimited(&buf[offset..])?;
          offset += len;
          member = Some(val);
        }
        other => {
          offset += 1;

          let (wire_type, _) = split(other);
          let wire_type = WireType::try_from(wire_type).map_err(DecodeError::unknown_wire_type)?;
          offset += skip(wire_type, &buf[offset..])?;
        }
      }
    }

    let member = member.ok_or(DecodeError::missing_field(
      "ConflictResponseMessage",
      "member",
    ))?;
    Ok((offset, Self { member }))
  }
}

impl<I, A> Data for ConflictResponseMessage<I, A>
where
  I: Data,
  A: Data,
{
  type Ref<'a> = ConflictResponseMessageRef<'a, I::Ref<'a>, A::Ref<'a>>;

  fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError>
  where
    Self: Sized,
  {
    Ok(Self {
      member: Member::from_ref(val.member)?,
    })
  }

  fn encoded_len(&self) -> usize {
    ConflictResponseMessageBorrow::from(self).encoded_len()
  }

  fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
    ConflictResponseMessageBorrow::from(self).encode_in(buf)
  }
}
