use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    env,
    ffi::OsStr,
    fmt, fs, iter,
    path::PathBuf,
    process,
    str::FromStr,
    time::Duration,
};

use clap::Clap;
use cloudformatious::{
    change_set::ChangeSet, ApplyStackError, ApplyStackInput, Capability, CloudFormatious,
    DeleteStackError, DeleteStackInput, Parameter, StackEvent, StackStatus, Status,
    StatusSentiment, TemplateSource,
};
use colored::{ColoredString, Colorize};
use futures_util::{Stream, StreamExt};
use rusoto_cloudformation::{CloudFormationClient, Tag};
use rusoto_core::{HttpClient, Region};
use rusoto_credential::{
    AutoRefreshingProvider, ChainProvider, ProfileProvider, ProvideAwsCredentials,
};

const MISSING_REGION: &str = "Unable to determine AWS region.
You can set it in your profile, assign `AWS_REGION`, or supply `--region`.";
const AWS_CLOUDFORMATION_STACK: &str = "AWS::CloudFormation::Stack";
const SHORT_UPDATE_COMPLETE_CLEANUP_IN_PROGRESS: &str = "UPDATE_CLEANUP_IN_PROGRESS";
const SHORT_UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS: &str = "ROLLBACK_CLEANUP_IN_PROGRESS";

/// A CloudFormation CLI that won't make you cry.
///
/// All commands will look for AWS configuration in the usual places. See AWS CLI documentation for
/// more information: https://docs.aws.amazon.com/cli/latest/topic/config-vars.html
///
/// Use `cloudformatious <command> --help` to get more information about individual commands.
#[derive(Clap, Debug)]
struct Args {
    /// Disable informational output to STDERR.
    #[clap(long)]
    quiet: bool,

    /// The region to use. Overrides config/env settings.
    #[clap(long, env = "AWS_REGION")]
    region: Option<Region>,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Clap, Debug)]
enum Command {
    ApplyStack(ApplyStackArgs),
    DeleteStack(DeleteStackArgs),
}

/// Apply a CloudFormation template.
///
/// This performs an update or create operation for a target stack. It's not an error for there
/// to be no changes. The command runs until the stack settles.
///
/// # Output
///
/// Stack events are printed to STDERR as the operation proceeds, unless disabled with `--quiet`.
///
/// If the stack operation succeeds and there are no resource errors, then the stack's outputs
/// are printed to STDOUT as JSON.
///
/// If the stack operation succeeds and there *are* resource errors, then details of the errors
/// are printed to STDERR and the stack's outputs are printed to STDOUT as JSON.
///
/// If the stack operation fails, then details of the error(s) are printed to STDERR.
///
/// # Exit code
///
/// If the stack operation succeeds and there are no resource errors, then the CLI will exit
/// successfully with code 0.
///
/// If the operation succeeds but there *are* resource errors, then the exit code is 3.
///
/// If the operation fails because the stack settled in an error state, then exit code is 4.
///
/// If the operation fails for any other reason, then the exit code is 1.
#[derive(Clap, Debug)]
struct ApplyStackArgs {
    /// Capabilities to explicitly acknowledge.
    #[clap(long)]
    capabilities: Vec<CapabilityArg>,

    /// A unique identifier for this `apply_stack` operation.
    #[clap(long)]
    client_request_token: Option<String>,

    /// The Simple Notification Service (SNS) topic ARNs to publish stack related events.
    #[clap(long)]
    notification_arns: Vec<String>,

    /// A list of input parameters for the stack.
    ///
    /// Parameters should be supplied as `key=value` strings.
    #[clap(long)]
    parameters: Vec<ParameterArg>,

    /// The template resource types that you have permissions to work with for this `apply_stack`
    /// operation, such as `AWS::EC2::Instance`, `AWS::EC2::*`, or `Custom::MyCustomInstance`.
    #[clap(long)]
    resource_types: Vec<String>,

    /// The Amazon Resource Name (ARN) of an AWS Identity And Access Management (IAM) role that AWS
    /// CloudFormation assumes to apply the stack.
    #[clap(long)]
    role_arn: Option<String>,

    /// The name that is associated with the stack.
    ///
    /// If this isn't set explicitly then the file name of the `template_path` is used as the stack
    /// name. E.g. if `template_path` is `deployment/cloudformation/my-stack.yaml` then the default
    /// stack name would be `my-stack`.
    #[clap(long)]
    stack_name: Option<String>,

    /// Key-value pairs to associate with this stack.
    ///
    /// Tags should be supplied either as `key=value` strings and/or as a JSON object (e.g.
    /// `{"key1": "value1", "key2": "value2"}). JSON is tried first.
    #[clap(long)]
    tags: Vec<TagArg>,

    /// Path to the template to be applied.
    template_path: PathBuf,
}

impl TryFrom<ApplyStackArgs> for ApplyStackInput {
    type Error = Error;
    fn try_from(args: ApplyStackArgs) -> Result<Self, Self::Error> {
        let template_path = args.template_path;

        Ok(ApplyStackInput {
            capabilities: args.capabilities.into_iter().map(Into::into).collect(),
            client_request_token: args.client_request_token,
            notification_arns: args.notification_arns,
            parameters: args.parameters.into_iter().map(Into::into).collect(),
            resource_types: if args.resource_types.is_empty() {
                None
            } else {
                Some(args.resource_types)
            },
            role_arn: args.role_arn,
            stack_name: args.stack_name.unwrap_or_else(|| {
                template_path
                    .file_stem()
                    .unwrap_or_else(|| OsStr::new(""))
                    .to_string_lossy()
                    .to_string()
            }),
            tags: args.tags.into_iter().flatten().collect(),
            template_source: TemplateSource::inline(fs::read_to_string(&template_path).map_err(
                |error| {
                    Error::other(format!(
                        "Invalid template path {:?}: {}",
                        template_path, error
                    ))
                },
            )?),
        })
    }
}

/// Newtype for parsing capabilities.
// TODO: use impl Deserialize upstream and use `Capability` directly.
#[derive(Debug)]
struct CapabilityArg(Capability);

impl FromStr for CapabilityArg {
    type Err = InvalidCapability;
    fn from_str(capability: &str) -> Result<Self, Self::Err> {
        let capability = match capability {
            "CAPABILITY_IAM" => Capability::Iam,
            "CAPABILITY_NAMED_IAM" => Capability::NamedIam,
            "CAPABILITY_AUTO_EXPAND" => Capability::AutoExpand,
            _ => return Err(InvalidCapability(capability.to_string())),
        };
        Ok(Self(capability))
    }
}

impl From<CapabilityArg> for Capability {
    fn from(arg: CapabilityArg) -> Self {
        arg.0
    }
}

#[derive(Debug)]
struct InvalidCapability(String);

impl fmt::Display for InvalidCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid capability `{}`, should be one of `{}`, `{}`, or `{}`",
            self.0,
            Capability::Iam,
            Capability::NamedIam,
            Capability::AutoExpand
        )
    }
}

impl std::error::Error for InvalidCapability {}

/// Newtype for parsing parameters.
#[derive(Debug)]
struct ParameterArg(Parameter);

impl FromStr for ParameterArg {
    type Err = InvalidParameter;
    fn from_str(parameter: &str) -> Result<Self, Self::Err> {
        let kv: Vec<_> = parameter.splitn(2, '=').collect();
        let [key, value]: [_; 2] = kv
            .try_into()
            .map_err(|_| InvalidParameter(parameter.to_string()))?;
        Ok(Self(Parameter {
            key: key.to_string(),
            value: value.to_string(),
        }))
    }
}

impl From<ParameterArg> for Parameter {
    fn from(arg: ParameterArg) -> Self {
        arg.0
    }
}

#[derive(Debug)]
struct InvalidParameter(String);

impl fmt::Display for InvalidParameter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid parameter `{}`, must be in the form `key=value`",
            self.0
        )
    }
}

impl std::error::Error for InvalidParameter {}

/// Newtype for parsing tags.
#[derive(Debug)]
enum TagArg {
    KeyValue(Tag),
    Json(Vec<Tag>),
}

impl FromStr for TagArg {
    type Err = InvalidTag;
    fn from_str(tag: &str) -> Result<Self, Self::Err> {
        // First try to parse as JSON
        let tags: Result<HashMap<String, String>, _> = serde_json::from_str(tag);
        if let Ok(tags) = tags {
            return Ok(TagArg::Json(
                tags.into_iter()
                    .map(|(key, value)| Tag { key, value })
                    .collect(),
            ));
        }

        let kv: Vec<_> = tag.splitn(2, '=').collect();
        let [key, value]: [_; 2] = kv.try_into().map_err(|_| InvalidTag(tag.to_string()))?;
        Ok(Self::KeyValue(Tag {
            key: key.to_string(),
            value: value.to_string(),
        }))
    }
}

impl IntoIterator for TagArg {
    type Item = Tag;
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::KeyValue(tag) => vec![tag].into_iter(),
            Self::Json(tags) => tags.into_iter(),
        }
    }
}

#[derive(Debug)]
struct InvalidTag(String);

impl fmt::Display for InvalidTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid tag `{}`, must be in the form `key=value` or a JSON object",
            self.0
        )
    }
}

impl std::error::Error for InvalidTag {}

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
#[derive(Clap, Debug)]
struct DeleteStackArgs {
    /// A unique identifier for this `delete_stack` operation.
    #[clap(long)]
    client_request_token: Option<String>,

    /// For stacks in the `DELETE_FAILED` state, a list of resource logical IDs that are associated
    /// with the resources you want to retain. During deletion, AWS CloudFormation deletes the stack
    /// but does not delete the retained resources.
    #[clap(long)]
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

impl TryFrom<DeleteStackArgs> for DeleteStackInput {
    type Error = Error;
    fn try_from(args: DeleteStackArgs) -> Result<Self, Self::Error> {
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
                    .expect("bug: DeleteStackArgs without stack_name or template_path")
                    .file_stem()
                    .unwrap_or_else(|| OsStr::new(""))
                    .to_string_lossy()
                    .to_string()
            }),
        })
    }
}

#[derive(Debug)]
struct Error {
    kind: ErrorKind,
    error: Box<dyn std::error::Error>,
}

impl Error {
    fn warning<E: Into<Box<dyn std::error::Error>>>(error: E) -> Self {
        Self {
            kind: ErrorKind::Warning,
            error: error.into(),
        }
    }

    fn failure<E: Into<Box<dyn std::error::Error>>>(error: E) -> Self {
        Self {
            kind: ErrorKind::Failure,
            error: error.into(),
        }
    }

    fn other<E: Into<Box<dyn std::error::Error>>>(error: E) -> Self {
        Self {
            kind: ErrorKind::Other,
            error: error.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Debug)]
enum ErrorKind {
    /// Operation succeeded with warnings.
    Warning,

    /// Operation failed.
    Failure,

    /// Another kind of error occurred.
    Other,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if let Err(error) = try_main(args).await {
        eprintln!("{}", error);
        process::exit(match error.kind {
            ErrorKind::Warning => 3,
            ErrorKind::Failure => 4,
            ErrorKind::Other => 1,
        });
    }
}

async fn try_main(args: Args) -> Result<(), Error> {
    let client = get_client(
        args.region
            .map(Ok)
            .or_else(get_region)
            .ok_or_else(|| Error::other(MISSING_REGION))?
            .map_err(Error::other)?,
    )
    .await
    .map_err(Error::other)?;

    match args.command {
        Command::ApplyStack(cmd_args) => {
            let mut apply = client.apply_stack(cmd_args.try_into()?);

            let change_set = apply.change_set().await.map_err(Error::other)?;
            let sizing = Sizing::new_for_change_set(&change_set);

            if !args.quiet {
                print_events(&sizing, apply.events()).await;
            }

            let output = apply.await.map_err(|error| match &error {
                ApplyStackError::Warning { .. } => Error::warning(error),
                ApplyStackError::Failure { .. } => Error::failure(error),
                ApplyStackError::CloudFormationApi(_) => Error::other(error),
                ApplyStackError::CreateChangeSetFailed { .. } => Error::other(error),
            })?;

            let outputs_json: serde_json::Value = output
                .outputs
                .into_iter()
                .map(|output| (output.key, output.value.into()))
                .collect::<serde_json::Map<_, _>>()
                .into();
            println!(
                "{}",
                serde_json::to_string_pretty(&outputs_json).expect("oh no")
            );
        }
        Command::DeleteStack(cmd_args) => {
            let mut delete = client.delete_stack(cmd_args.try_into()?);
            let sizing = Sizing::default();

            if !args.quiet {
                print_events(&sizing, delete.events()).await;
            }

            delete.await.map_err(|error| match &error {
                DeleteStackError::Warning { .. } => Error::warning(error),
                DeleteStackError::Failure { .. } => Error::failure(error),
                DeleteStackError::CloudFormationApi(_) => Error::other(error),
            })?;
        }
    }

    Ok(())
}

fn get_region() -> Option<Result<Region, Box<dyn std::error::Error>>> {
    // rusoto_cloudformation::Region::default implements a similar algorithm but falls back to
    // us-east-1, which we don't want.
    match env::var("AWS_DEFAULT_REGION").or_else(|_| env::var("AWS_REGION")) {
        Ok(region) => Some(region.parse().map_err(Into::into)),
        Err(_) => match ProfileProvider::region() {
            Ok(Some(region)) => Some(region.parse().map_err(Into::into)),
            Ok(None) => None,
            Err(error) => Some(Err(error.into())),
        },
    }
}

async fn get_client(region: Region) -> Result<CloudFormationClient, Box<dyn std::error::Error>> {
    let client = HttpClient::new()?;

    let mut credentials = AutoRefreshingProvider::new(ChainProvider::new())?;
    credentials.get_mut().set_timeout(Duration::from_secs(1));

    // Proactively fetch credentials so we get earlier errors.
    credentials.credentials().await?;

    Ok(CloudFormationClient::new_with(client, credentials, region))
}

struct Sizing {
    resource_status: usize,
    logical_resource_id: usize,
    resource_type: usize,
}

impl Sizing {
    fn new_for_change_set(change_set: &ChangeSet) -> Self {
        let default = Self::default();
        Self {
            resource_status: default.resource_status,
            logical_resource_id: change_set
                .changes
                .iter()
                .map(|change| change.logical_resource_id.len())
                .chain(iter::once(change_set.stack_name.len()))
                .max()
                .unwrap(), // we insert the stack name so unwrap is fine
            resource_type: change_set
                .changes
                .iter()
                .map(|change| change.resource_type.len())
                .chain(iter::once(default.resource_type))
                .max()
                .unwrap(), // we insert the default so unwrap is fine
        }
    }
}

impl Default for Sizing {
    fn default() -> Self {
        Self {
            resource_status: SHORT_UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS.len(),
            logical_resource_id: 0,
            resource_type: AWS_CLOUDFORMATION_STACK.len(),
        }
    }
}

async fn print_events(sizing: &Sizing, mut events: impl Stream<Item = StackEvent> + Unpin) {
    while let Some(event) = events.next().await {
        eprintln!(
            "{} {:resource_status_size$} {:logical_resource_id_size$} {:resource_type_size$} {}",
            format!("{:?}", event.timestamp()).bright_black(),
            colorize_status(&event),
            event.logical_resource_id(),
            event.resource_type(),
            colorize_status_reason(
                event.resource_status(),
                event.resource_status_reason().unwrap_or("")
            ),
            resource_status_size = sizing.resource_status,
            logical_resource_id_size = sizing.logical_resource_id,
            resource_type_size = sizing.resource_type,
        );
    }
    eprintln!();
}

fn colorize_status(event: &StackEvent) -> ColoredString {
    let status = match event {
        StackEvent::Resource {
            resource_status, ..
        } => resource_status.to_string(),
        // Shorten the most verbose statuses for better formatting
        StackEvent::Stack {
            resource_status, ..
        } => match resource_status {
            StackStatus::UpdateCompleteCleanupInProgress => {
                SHORT_UPDATE_COMPLETE_CLEANUP_IN_PROGRESS.to_string()
            }
            StackStatus::UpdateRollbackCompleteCleanupInProgress => {
                SHORT_UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS.to_string()
            }
            _ => resource_status.to_string(),
        },
    };
    match event.resource_status().sentiment() {
        StatusSentiment::Positive => status.green(),
        StatusSentiment::Neutral => status.yellow(),
        StatusSentiment::Negative => status.red(),
    }
}

fn colorize_status_reason(status: &dyn Status, reason: &str) -> ColoredString {
    if status.sentiment().is_negative() {
        reason.red()
    } else {
        reason.bright_black()
    }
}
