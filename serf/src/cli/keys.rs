use clap::Args;

use super::RpcArgs;

/// Manage the internal encryption keyring used by Serf. Modifications made by
/// this command will be broadcasted to all members in the cluster and applied
/// locally on each member. Operations of this command are idempotent.
///
/// To facilitate key rotation, Serf allows for multiple encryption keys to be in
/// use simultaneously. Only one key, the "primary" key, will be used for
/// encrypting messages. All other keys are used for decryption only.
///
/// All variations of this command will return 0 if all nodes reply and report
/// no errors. If any node fails to respond or reports failure, we return 1.
///
/// WARNING: Running with multiple encryption keys enabled is recommended as a
/// transition state only. Performance may be impacted by using multiple keys.
#[derive(Debug, Args)]
pub struct KeysArgs {
  /// Install a new key onto Serf's internal keyring. This
  /// will enable the key for decryption. The key will not
  /// be used to encrypt messages until the primary key is
  /// changed.
  #[arg(short, long)]
  pub install: String,

  /// Change the primary key used for encrypting messages.
  /// All nodes in the cluster must already have this key
  /// installed if they are to continue communicating with
  /// eachother.
  #[arg(short, long = "use")]
  pub use_: String,

  /// Remove a key from Serf's internal keyring. The key
  /// being removed may not be the current primary key.
  #[arg(short, long)]
  pub remove: String,

  /// List all currently known keys in the cluster. This
  /// will ask all nodes in the cluster for a list of keys
  /// and dump a summary containing each key and the
  /// number of members it is installed on to the console.
  #[arg(short, long, default_value_t = false)]
  pub list: bool,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
