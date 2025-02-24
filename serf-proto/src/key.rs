use indexmap::IndexMap;
use memberlist_proto::{SecretKey, SecretKeys};
use smol_str::SmolStr;

/// KeyRequest is used to contain input parameters which get broadcasted to all
/// nodes as part of a key query operation.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct KeyRequestMessage {
  /// The secret key
  #[viewit(
    getter(const, attrs(doc = "Returns the secret key")),
    setter(const, attrs(doc = "Sets the secret key (Builder pattern)"))
  )]
  key: Option<SecretKey>,
}

/// Key response message
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
#[cfg(feature = "encryption")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct KeyResponseMessage {
  /// Indicates true/false if there were errors or not
  #[viewit(
    getter(const, attrs(doc = "Returns true/false if there were errors or not")),
    setter(
      const,
      attrs(doc = "Sets true/false if there were errors or not (Builder pattern)")
    )
  )]
  result: bool,
  /// Contains error messages or other information
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(doc = "Returns the error messages or other information")
    ),
    setter(attrs(doc = "Sets the error messages or other information (Builder pattern)"))
  )]
  message: SmolStr,
  /// Used in listing queries to relay a list of installed keys
  #[viewit(
    getter(const, style = "ref", attrs(doc = "Returns a list of installed keys")),
    setter(attrs(doc = "Sets the the list of installed keys (Builder pattern)"))
  )]
  keys: SecretKeys,
  /// Used in listing queries to relay the primary key
  #[viewit(
    getter(const, attrs(doc = "Returns the primary key")),
    setter(attrs(doc = "Sets the primary key (Builder pattern)"))
  )]
  primary_key: Option<SecretKey>,
}

impl KeyResponseMessage {
  /// Adds a key to the list of keys
  #[inline]
  pub fn add_key(&mut self, key: SecretKey) -> &mut Self {
    self.keys.push(key);
    self
  }
}

/// KeyResponse is used to relay a query for a list of all keys in use.
#[viewit::viewit(setters(prefix = "with"))]
#[derive(Default)]
pub struct KeyResponse<I> {
  /// Map of node id to response message
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(doc = "Returns the map of node id to response message")
    ),
    setter(attrs(doc = "Sets the map of node id to response message (Builder pattern)"))
  )]
  messages: IndexMap<I, SmolStr>,
  /// Total nodes memberlist knows of
  #[viewit(
    getter(const, attrs(doc = "Returns the total nodes memberlist knows of")),
    setter(
      const,
      attrs(doc = "Sets total nodes memberlist knows of (Builder pattern)")
    )
  )]
  num_nodes: usize,
  /// Total responses received
  #[viewit(
    getter(const, attrs(doc = "Returns the total responses received")),
    setter(
      const,
      attrs(doc = "Sets the total responses received (Builder pattern)")
    )
  )]
  num_resp: usize,
  /// Total errors from request
  #[viewit(
    getter(const, attrs(doc = "Returns the total errors from request")),
    setter(
      const,
      attrs(doc = "Sets the total errors from request (Builder pattern)")
    )
  )]
  num_err: usize,

  /// A mapping of the value of the key bytes to the
  /// number of nodes that have the key installed.
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(
        doc = "Returns a mapping of the value of the key bytes to the number of nodes that have the key installed."
      )
    ),
    setter(attrs(
      doc = "Sets a mapping of the value of the key bytes to the number of nodes that have the key installed (Builder pattern)"
    ))
  )]
  keys: IndexMap<SecretKey, usize>,

  /// A mapping of the value of the primary
  /// key bytes to the number of nodes that have the key installed.
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(
        doc = "Returns a mapping of the value of the primary key bytes to the number of nodes that have the key installed."
      )
    ),
    setter(attrs(
      doc = "Sets a mapping of the value of the primary key bytes to the number of nodes that have the key installed. (Builder pattern)"
    ))
  )]
  primary_keys: IndexMap<SecretKey, usize>,
}

/// KeyRequestOptions is used to contain optional parameters for a keyring operation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct KeyRequestOptions {
  /// The number of duplicate query responses to send by relaying through
  /// other nodes, for redundancy
  pub relay_factor: u8,
}
