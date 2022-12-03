use std::{collections::HashMap, convert::TryInto, fmt, path::PathBuf, str::FromStr};

use aws_types::region::Region;
use cloudformatious::{
    self, ApplyStackError, ApplyStackInput, BlockedStackStatus, Capability, Client,
    DeleteStackError, DeleteStackInput, Parameter, Tag, TemplateSource,
};
use futures_util::TryFutureExt;

use crate::{
    client::get_config,
    fmt::{print_events, Sizing},
    package, s3, Error, Template,
};

/// Apply a CloudFormation template.
///
/// This performs an update or create operation for a target stack. It's not an error for there
/// to be no changes. The command runs until the stack settles.
///
/// # Preprocessing
///
/// The template will be pre-processed for references to local paths in the following properties:
///
/// - `AWS::Lambda::Function`: `Code`
///
/// If local paths are found, they will be zipped and uploaded to S3 based on `--package-bucket`
/// and `--package-prefix`. `--package-bucket` is required if the template contains any local paths.
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
#[derive(Debug, clap::Parser)]
pub struct Args {
    /// Capabilities to explicitly acknowledge.
    #[clap(long, num_args(1..))]
    capabilities: Vec<CapabilityArg>,

    /// A unique identifier for this `apply_stack` operation.
    #[clap(long)]
    client_request_token: Option<String>,

    /// The Simple Notification Service (SNS) topic ARNs to publish stack related events.
    #[clap(long, num_args(1..))]
    notification_arns: Vec<String>,

    /// The S3 bucket to upload packages to.
    ///
    /// Not required unless there are references to local paths in the template.
    #[clap(long)]
    package_bucket: Option<String>,

    /// A prefix for any uploaded packages.
    #[clap(long)]
    package_prefix: Option<String>,

    /// A list of input parameters for the stack.
    #[clap(long, num_args(1..), value_name("KEY=VALUE"))]
    parameters: Vec<ParameterArg>,

    /// Disable informational output to STDERR.
    #[clap(long)]
    quiet: bool,

    /// The template resource types that you have permissions to work with for this `apply_stack`
    /// operation, such as `AWS::EC2::Instance`, `AWS::EC2::*`, or `Custom::MyCustomInstance`.
    #[clap(long, num_args(1..))]
    resource_types: Vec<String>,

    /// The Amazon Resource Name (ARN) of an AWS Identity And Access Management (IAM) role that AWS
    /// CloudFormation assumes to apply the stack.
    #[clap(long)]
    role_arn: Option<String>,

    /// The name that is associated with the stack.
    #[clap(long)]
    stack_name: String,

    /// Key-value pairs to associate with this stack.
    ///
    /// Tags should be supplied either as `key=value` strings and/or as a JSON object (e.g.
    /// `{"key1": "value1", "key2": "value2"}).
    #[clap(long, num_args(1..), value_name("KEY=VALUE|JSON"))]
    tags: Vec<TagArg>,

    /// Path to the template to be applied.
    #[clap(long)]
    template_path: PathBuf,
}

impl Args {
    fn into_input(self, template: &Template) -> ApplyStackInput {
        ApplyStackInput {
            capabilities: self.capabilities.into_iter().map(Into::into).collect(),
            client_request_token: self.client_request_token,
            disable_rollback: false,
            notification_arns: self.notification_arns,
            parameters: self.parameters.into_iter().map(Into::into).collect(),
            resource_types: if self.resource_types.is_empty() {
                None
            } else {
                Some(self.resource_types)
            },
            role_arn: self.role_arn,
            stack_name: self.stack_name,
            tags: self.tags.into_iter().flatten().collect(),
            template_source: TemplateSource::inline(template.to_string()),
        }
    }
}

pub async fn main(region: Option<Region>, args: Args) -> Result<(), Error> {
    let quiet = args.quiet;

    let mut template = Template::open(args.template_path.clone()).await?;
    preprocess(region.as_ref(), &args, &mut template).await?;

    let config = get_config(region).await;
    let client = cloudformatious::Client::new(&config);
    let input = args.into_input(&template);
    let mut apply = client.apply_stack(input.clone());

    let change_set = match apply.change_set().await {
        Ok(change_set) => Ok(change_set),
        Err(ApplyStackError::Blocked { status }) => {
            recover(status, &client, &input, quiet).await?;

            apply = client.apply_stack(input.clone());
            apply.change_set().await.map_err(Error::other)
        }
        Err(error) => Err(Error::other(error)),
    }?;
    let sizing = Sizing::new_for_change_set(&change_set);

    if !quiet {
        print_events(&sizing, apply.events()).await;
    }

    let output = apply
        .or_else(|error| async {
            let ApplyStackError::Blocked { status } = error else {
                return Err(Error::other(error));
            };

            recover(status, &client, &input, quiet).await?;

            let mut apply = client.apply_stack(input);

            if !quiet {
                print_events(&sizing, apply.events()).await;
            }

            apply.await.map_err(|error| match error {
                ApplyStackError::Warning { warning, .. } => Error::Warning(warning),
                ApplyStackError::Failure(failure) => Error::Failure(failure),
                error => Error::other(error),
            })
        })
        .await?;

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

    Ok(())
}

async fn preprocess(
    region: Option<&Region>,
    args: &Args,
    template: &mut Template,
) -> Result<(), Error> {
    let mut targets = package::targets(template).peekable();
    if targets.peek().is_none() {
        return Ok(());
    }

    let package_bucket = if let Some(bucket) = args.package_bucket.as_deref() {
        bucket
    } else {
        drop(targets); // it's not clear why this is necessary, but without it the use of `template`
                       // below is an error
        return Err(Error::other(format!(
            concat!(
                "the `--package-bucket` option is required because template `{}` contains ",
                "references to local paths that will be packaged"
            ),
            template.source()
        )));
    };

    let client = s3::Client::new(region.cloned()).await;

    package::process(
        &client,
        package_bucket,
        args.package_prefix.as_deref(),
        targets,
    )
    .await?;

    Ok(())
}

async fn recover(
    status: BlockedStackStatus,
    client: &Client,
    input: &ApplyStackInput,
    quiet: bool,
) -> Result<(), Error> {
    match status {
        BlockedStackStatus::RollbackComplete => {
            eprintln!("Stack is in state {} – deleting it first", status);
            // From ROLLBACK_COMPLETE all we can do is delete the stack.
            let mut delete_input = DeleteStackInput::new(&input.stack_name);
            delete_input.role_arn = input.role_arn.clone();

            let mut delete = client.delete_stack(delete_input);
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
        status => Err(Error::other(format!(
            "Can't apply stack in block status {}",
            status
        ))),
    }
}

/// Newtype for parsing capabilities.
// TODO: use impl Deserialize upstream and use `Capability` directly.
#[derive(Clone, Copy, Debug)]
pub struct CapabilityArg(Capability);

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
pub struct InvalidCapability(String);

impl fmt::Display for InvalidCapability {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
#[derive(Clone, Debug)]
pub struct ParameterArg(Parameter);

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
pub struct InvalidParameter(String);

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
#[derive(Clone, Debug)]
pub enum TagArg {
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
pub struct InvalidTag(String);

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
