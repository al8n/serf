use memberlist_proto::{Data, DataRef, DecodeError, EncodeError, WireType};

/// Delegate version
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash, derive_more::IsVariant, derive_more::Display)]
#[cfg_attr(feature = "serde", derive(::serde::Serialize, ::serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum DelegateVersion {
  /// Version 1
  #[default]
  #[display("v1")]
  V1,
  /// Unknown version (used for forwards and backwards compatibility)
  #[display("unknown({_0})")]
  Unknown(u8),
}

impl From<u8> for DelegateVersion {
  fn from(v: u8) -> Self {
    match v {
      1 => Self::V1,
      val => Self::Unknown(val),
    }
  }
}

impl From<DelegateVersion> for u8 {
  fn from(v: DelegateVersion) -> Self {
    match v {
      DelegateVersion::V1 => 1,
      DelegateVersion::Unknown(val) => val,
    }
  }
}

/// Protocol version
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash, derive_more::IsVariant, derive_more::Display)]
#[cfg_attr(feature = "serde", derive(::serde::Serialize, ::serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum ProtocolVersion {
  /// Version 1
  #[default]
  #[display("v1")]
  V1,
  /// Unknown version (used for forwards and backwards compatibility)
  #[display("unknown({_0})")]
  Unknown(u8),
}

impl From<u8> for ProtocolVersion {
  fn from(v: u8) -> Self {
    match v {
      1 => Self::V1,
      val => Self::Unknown(val),
    }
  }
}

impl From<ProtocolVersion> for u8 {
  fn from(v: ProtocolVersion) -> Self {
    match v {
      ProtocolVersion::V1 => 1,
      ProtocolVersion::Unknown(val) => val,
    }
  }
}

macro_rules! impl_data {
  ($($ty:ty),+$(,)?) => {
    $(
      impl<'a> DataRef<'a, Self> for $ty {
        fn decode(src: &'a [u8]) -> Result<(usize, Self), DecodeError> {
          if src.is_empty() {
            return Err(DecodeError::buffer_underflow());
          }

          Ok((1, Self::from(src[0])))
        }
      }

      impl Data for $ty {
        const WIRE_TYPE: WireType = WireType::Byte;

        type Ref<'a> = Self;

        fn from_ref(val: Self::Ref<'_>) -> Result<Self, DecodeError> {
          Ok(val)
        }

        #[inline]
        fn encoded_len(&self) -> usize {
          1
        }

        #[inline]
        fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
          if buf.is_empty() {
            return Err(EncodeError::insufficient_buffer(1, 0));
          }

          buf[0] = u8::from(*self);
          Ok(1)
        }
      }
    )*
  };
}

impl_data!(DelegateVersion, ProtocolVersion);

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(feature = "arbitrary")]
  use arbitrary::{Arbitrary, Unstructured};

  #[test]
  #[cfg(feature = "arbitrary")]
  fn test_delegate_version() {
    let mut buf = [0; 64];
    rand::fill(&mut buf[..]);

    let mut data = Unstructured::new(&buf);
    let _ = DelegateVersion::arbitrary(&mut data).unwrap();

    assert_eq!(u8::from(DelegateVersion::V1), 1u8);
    assert_eq!(DelegateVersion::V1.to_string(), "V1");
    assert_eq!(DelegateVersion::Unknown(2).to_string(), "Unknown(2)");
    assert_eq!(DelegateVersion::from(1), DelegateVersion::V1);
    assert_eq!(DelegateVersion::from(2), DelegateVersion::Unknown(2));
  }

  #[test]
  #[cfg(feature = "arbitrary")]
  fn test_protocol_version() {
    let mut buf = [0; 64];
    rand::fill(&mut buf[..]);

    let mut data = Unstructured::new(&buf);
    let _ = ProtocolVersion::arbitrary(&mut data).unwrap();
    assert_eq!(u8::from(ProtocolVersion::V1), 1);
    assert_eq!(ProtocolVersion::V1.to_string(), "V1");
    assert_eq!(ProtocolVersion::Unknown(2).to_string(), "Unknown(2)");
    assert_eq!(ProtocolVersion::from(1), ProtocolVersion::V1);
    assert_eq!(ProtocolVersion::from(2), ProtocolVersion::Unknown(2));
  }
}
