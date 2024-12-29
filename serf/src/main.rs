use clap::Parser;

use serf::cli::{Cli, Commands};

fn main() {
  let cli = Cli::parse();

  // match cli.commands {
  //   Commands::Info(info_args) => {},
  //   Commands::Event(event_args) => {},
  //   Commands::Join(join_args) => {},
  //   Commands::ForceLeave(force_leave_args) => {},
  // }
}
