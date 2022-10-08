mod client;
mod command;
mod error;
mod fmt;
mod package;
mod s3;
mod template;

use std::process;

use clap::Parser;
use rusoto_core::Region;

use self::{client::get_client, error::Error, template::Template};

/// A CloudFormation CLI that won't make you cry.
///
/// All commands will look for AWS configuration in the usual places. See AWS CLI documentation for
/// more information: <https://docs.aws.amazon.com/cli/latest/topic/config-vars.html>
///
/// Use `cloudformatious <command> --help` to get more information about individual commands.
#[derive(Parser, Debug)]
#[clap(name = "cloudformatious")]
struct Args {
    /// The region to use. Overrides config/env settings.
    #[clap(long, env = "AWS_REGION")]
    region: Option<Region>,

    #[clap(subcommand)]
    command: command::Command,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if let Err(error) = command::main(args.region, args.command).await {
        eprintln!("{}", error);
        process::exit(match error {
            Error::Warning(_) => 3,
            Error::Failure(_) => 4,
            Error::Other(_) => 1,
        });
    }
}
