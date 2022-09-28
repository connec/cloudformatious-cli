use std::{
    env,
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
};

use clap::CommandFactory as _;

use crate::Error;

/// Write a shell completion script to STDOUT.
#[derive(Debug, clap::Parser)]
pub struct Args {
    /// The shell to generate completions for.
    ///
    /// Determined from $SHELL if left unset.
    #[clap(value_enum)]
    pub shell: Option<clap_complete::Shell>,
}

pub fn main(args: Args) -> Result<(), Error> {
    let mut cmd = crate::Args::command();
    let cmd_name = cmd.get_name().to_string();
    if let Some(shell) = args.shell.or_else(default_shell) {
        clap_complete::generate(shell, &mut cmd, cmd_name, &mut io::stdout());
        Ok(())
    } else {
        let shell_env = env::var("SHELL");
        Err(Error::other(format!(
            "unable to determine current shell from $SHELL ({})",
            shell_env.as_deref().unwrap_or("not set"),
        )))
    }
}

fn default_shell() -> Option<clap_complete::Shell> {
    let shell = env::var("SHELL").ok().map(PathBuf::from);
    shell
        .as_deref()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .and_then(|shell_name| shell_name.parse().ok())
}
