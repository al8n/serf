use memberlist_proto::{Data, DataRef, DecodeError};

/// The decoder for ids
pub struct IdDecoder<'a, I> {
  src: &'a [u8],
  len: usize,
  offset: usize,
  has_err: bool,
  _phantom: std::marker::PhantomData<I>,
}

impl<I> Clone for IdDecoder<'_, I> {
  fn clone(&self) -> Self {
    *self
  }
}

impl<I> Copy for IdDecoder<'_, I> {}

impl<I> core::fmt::Debug for IdDecoder<'_, I> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IdDecoder")
      .field("src", &self.src)
      .field("offset", &self.offset)
      .finish()
  }
}

impl<'a, I> IdDecoder<'a, I> {
  pub(super) const fn new(src: &'a [u8]) -> Self {
    Self {
      src,
      offset: 0,
      len: src.len(),
      has_err: false,
      _phantom: std::marker::PhantomData,
    }
  }
}

impl<'a, I> Iterator for IdDecoder<'a, I>
where
  I: Data,
{
  type Item = Result<I::Ref<'a>, DecodeError>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.has_err || self.offset >= self.len {
      return None;
    }

    Some(
      <I::Ref<'_> as DataRef<'_, I>>::decode_length_delimited(&self.src[self.offset..])
        .inspect_err(|_| {
          self.has_err = true;
        })
        .map(|(read, value)| {
          self.offset += read;
          value
        }),
    )
  }
}
