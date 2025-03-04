# Releases

## 0.3.0

### Features

- Redesign `Delegate` trait, making it easier to implement for users.
- Rewriting encoding/decoding to support forward and backward compitibility.
- Support `zstd`, `brotli`, `lz4`, and `snappy` for compressing.
- Support `crc32`, `xxhash64`, `xxhash32`, `xxhash3`, `murmur3` for checksuming.
- Unify returned error, all exported APIs return `Error` on `Result::Err`.

### Example

- Add [`toyconsul`](./examples/toyconsul/) Example

### Breakage

- Remove `native-tls` supports
- Remove `s2n-quic` supports
- Remove `TransformDelegate` trait to simplify `Delegate` trait
- Remove `JoinError`, add an new `Error::Multiple` variant

### Testing

- Add fuzzy testing for encoding/decoding
