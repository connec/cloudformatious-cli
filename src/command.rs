mod apply_stack;
mod completions;
mod delete_stack;

use rusoto_core::Region;

use crate::Error;

#[derive(Debug, clap::Parser)]
pub enum Command {
    Completions(self::completions::Args),
    ApplyStack(self::apply_stack::Args),
    DeleteStack(self::delete_stack::Args),
}

pub async fn main(region: Option<Region>, command: Command) -> Result<(), Error> {
    match command {
        Command::Completions(args) => self::completions::main(args),
        Command::ApplyStack(args) => self::apply_stack::main(region, args).await,
        Command::DeleteStack(args) => self::delete_stack::main(region, args).await,
    }
}
