use std::{
    convert::{TryFrom, TryInto},
    ffi::OsStr,
    path::PathBuf,
};

use aws_types::region::Region;
use cloudformatious::{self, DeleteStackError, DeleteStackInput};

use crate::{
    client::get_config,
    fmt::{print_events, Sizing},
    Error,
};

/// Delete a CloudFormation stack.
///
/// # Output
///
/// Stack events are printed to STDERR as the operation proceeds, unless disable with `--quiet`.
///
/// If the stack is deleted successfully and there are no resource errors, or if no stack
/// existed in the first place, a confirmation message is printed to STDERR.
///
/// If the stack is deleted successfully and there *are* resource errors, then details of the
/// errors are printed to STDERR.
///
/// If the stack deletion fails, then details of the error(s) are printed to STDERR.
///
/// # Exit code
///
/// If the stack is deleted successfully and there are no resource errors, or if no stack
/// existed in the first place, then the CLI will exit successfully with code 0.
///
/// If the stack is deleted successfully but there *are* resource errors, then the exit code is
/// 3.
///
/// If the stack deletion fails because the stack settled in an error state, then exit code is
/// 4.
///
/// If the deletion fails for any other reason, then the exit code is 1.
#[derive(Debug, clap::Parser)]
pub struct Args {
    /// A unique identifier for this `delete_stack` operation.
    #[clap(long)]
    client_request_token: Option<String>,

    /// Disable informational output to STDERR.
    #[clap(long)]
    quiet: bool,

    /// For stacks in the `DELETE_FAILED` state, a list of resource logical IDs that are associated
    /// with the resources you want to retain. During deletion, AWS CloudFormation deletes the stack
    /// but does not delete the retained resources.
    #[clap(long, num_args(1..))]
    retain_resources: Vec<String>,

    /// The Amazon Resource Name (ARN) of an AWS Identity And Access Management (IAM) role that AWS
    /// CloudFormation assumes to delete the stack.
    #[clap(long)]
    role_arn: Option<String>,

    /// The name of the stack to delete.
    #[clap(long)]
    stack_name: Option<String>,

    /// The path to the template whose associated stack will be deleted.
    ///
    /// The stack to delete is determined from the file name of `template_path`. E.g. if
    /// `template_path` is `deployment/cloudformation/my-stack.yaml` then the default `my-stack`
    /// will be deleted.
    #[clap(required_unless_present = "stack-name")]
    template_path: Option<PathBuf>,
}

impl TryFrom<Args> for DeleteStackInput {
    type Error = Error;
    fn try_from(args: Args) -> Result<Self, Self::Error> {
        let template_path = args.template_path;
        Ok(DeleteStackInput {
            client_request_token: args.client_request_token,
            retain_resources: if args.retain_resources.is_empty() {
                None
            } else {
                Some(args.retain_resources)
            },
            role_arn: args.role_arn,
            stack_name: args.stack_name.unwrap_or_else(|| {
                template_path
                    .expect("bug: delete_stack::Args without stack_name or template_path")
                    .file_stem()
                    .unwrap_or_else(|| OsStr::new(""))
                    .to_string_lossy()
                    .to_string()
            }),
        })
    }
}

pub async fn main(region: Option<Region>, args: Args) -> Result<(), Error> {
    let quiet = args.quiet;

    let config = get_config(region).await;
    let client = cloudformatious::Client::new(&config);
    let mut delete = client.delete_stack(args.try_into()?);
    let sizing = Sizing::default();

    if !quiet {
        print_events(&sizing, delete.events()).await;
    }

    delete.await.map_err(|error| match error {
        DeleteStackError::Warning(warning) => Error::Warning(warning),
        DeleteStackError::Failure(failure) => Error::Failure(failure),
        DeleteStackError::CloudFormationApi(_) => Error::other(error),
    })?;

    Ok(())
}
