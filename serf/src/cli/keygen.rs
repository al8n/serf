use clap::Args;

/// Generates a new encryption key that can be
/// used to configure the
/// agent to encrypt traffic. The output of this command is already
/// in the proper format that the agent expects.
#[derive(Args, Debug)]
pub struct KeygenArgs {}
