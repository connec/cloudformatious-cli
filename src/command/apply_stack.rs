use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    ffi::OsStr,
    fmt, fs,
    path::PathBuf,
    str::FromStr,
};

use cloudformatious::{
    ApplyStackError, ApplyStackInput, Capability, CloudFormatious as _, Parameter, TemplateSource,
};
use rusoto_cloudformation::Tag;
use rusoto_core::Region;

use crate::{
    fmt::{print_events, Sizing},
    Error,
};

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
#[derive(Debug, clap::Parser)]
pub struct Args {
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

    /// Disable informational output to STDERR.
    #[clap(long)]
    quiet: bool,

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

impl TryFrom<Args> for ApplyStackInput {
    type Error = Error;
    fn try_from(args: Args) -> Result<Self, Self::Error> {
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

pub async fn main(region: Option<Region>, args: Args) -> Result<(), Error> {
    let quiet = args.quiet;

    let client = crate::get_client(region).await?;
    let mut apply = client.apply_stack(args.try_into()?);

    let change_set = apply.change_set().await.map_err(Error::other)?;
    let sizing = Sizing::new_for_change_set(&change_set);

    if !quiet {
        print_events(&sizing, apply.events()).await;
    }

    let output = apply.await.map_err(|error| match error {
        ApplyStackError::Warning { warning, .. } => Error::Warning(warning),
        ApplyStackError::Failure(failure) => Error::Failure(failure),
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

    Ok(())
}

/// Newtype for parsing capabilities.
// TODO: use impl Deserialize upstream and use `Capability` directly.
#[derive(Debug)]
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
#[derive(Debug)]
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
#[derive(Debug)]
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