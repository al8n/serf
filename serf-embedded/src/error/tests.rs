use serf_proto::options::Options as SerfOptions;

use super::InitError;

#[test]
fn display_prefixes_the_serf_options_arm() {
  let invalid = SerfOptions::new()
    .with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1)
    .validate()
    .expect_err("an over-ceiling max_user_event_size must fail validation");
  let e = InitError::InvalidSerfOptions(invalid);
  let rendered = std::format!("{e}");
  assert!(
    rendered.starts_with("invalid serf options: "),
    "the serf-options arm names its surface: {rendered}"
  );
  assert!(
    rendered.contains("max_user_event_size"),
    "the inner cause is carried through: {rendered}"
  );
}

#[test]
fn source_exposes_the_inner_error() {
  use core::error::Error as _;

  let invalid = SerfOptions::new()
    .with_max_user_event_size(SerfOptions::DEFAULT_USER_EVENT_SIZE_LIMIT + 1)
    .validate()
    .expect_err("an over-ceiling max_user_event_size must fail validation");
  let e = InitError::InvalidSerfOptions(invalid);
  assert!(e.source().is_some(), "the chained cause is preserved");
}
