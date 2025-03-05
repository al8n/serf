use clap::Parser;

use serf::cli::{Cli, Commands};

fn main() {
  let cli = Cli::parse();

  let res = cli.exec();
  if let Err(e) = res {
    eprintln!("{}", e);
    std::process::exit(1);
  }
}
