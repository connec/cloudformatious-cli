use std::{fmt, time::Duration};

use async_stream::try_stream;
use chrono::Utc;
use pin_utils::pin_mut;
use rusoto_cloudformation::{
    CloudFormation, CloudFormationClient, CreateChangeSetInput, DeleteStackInput,
    DescribeChangeSetInput, DescribeStackResourcesInput, ExecuteChangeSetInput, Parameter,
};
use tokio::sync::oneshot;
use tokio_stream::{Stream, StreamExt};

use crate::{
    events::{last_stack_event, stack_events_since},
    DeployInput, Error, Result, StackEvent,
};

/// Describes a set of changes that will be applied during deployment.
///
/// CloudFormation doesn't describe stack deletions with change sets. To keep the API consistent, we
/// augment our `ChangeSet` with an overall [`Effect`] on the stack, which includes
/// [`Delete`](Effect::Delete) as a possibility. We emulate the [`ResourceChange`]s for deletions by
/// using `DescribeStackResources` and assuming they will all be
/// [`Remove`](ResourceAction::Remove)d.
///
/// See [`Deploy::change_sets`](crate::Deploy::change_sets) for how to generate these during
/// deployment.
#[derive(Clone, Debug)]
pub struct ChangeSet {
    /// The aggregate effect of the change set on the stack.
    pub effect: Effect,

    /// The name of the stack the change set will be applied to.
    pub stack_name: String,

    /// The ID of the stack the change set will be applied to.
    pub stack_id: String,

    /// The changes that will be applied to resources during deployment.
    pub resource_changes: Vec<ResourceChange>,
}

/// The aggregate affect a [`ChangeSet`] will have on a CloudFormation stack.
#[derive(Clone, Debug)]
pub enum Effect {
    /// The change set will do nothing.
    Skip,

    /// The change set will create a new stack.
    Create { id: String },

    /// The change set will update an existing stack.
    Update { id: String },

    /// The change set will delete an existing stack.
    Delete,
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skip => "Skip",
            Self::Create { .. } => "Create",
            Self::Update { .. } => "Update",
            Self::Delete => "Delete",
        }
        .fmt(f)
    }
}

/// Describes a single change in a [`ChangeSet`].
#[derive(Clone, Debug)]
pub struct ResourceChange {
    /// The action that will be applied.
    pub action: ResourceAction,

    /// The logical resource ID of the affected resource.
    pub logical_resource_id: String,

    /// The physical resource ID of the affected resource.
    ///
    /// This will be `None` for new resources or resources in an error state.
    pub physical_resource_id: Option<String>,

    /// The type of the affected resource.
    pub resource_type: String,
}

impl ResourceChange {
    fn from_native(change: rusoto_cloudformation::Change) -> Self {
        let change = change
            .resource_change
            .expect("Change without resource_change");
        Self {
            action: change
                .action
                .expect("ResourceChange without action")
                .parse()
                .expect("unknown ResourceChange action"),
            logical_resource_id: change
                .logical_resource_id
                .expect("ResourceChange without logical_resource_id"),
            physical_resource_id: change.physical_resource_id,
            resource_type: change
                .resource_type
                .expect("ResourceChange without resource_type"),
        }
    }
}

/// An action that CloudFormation will apply during deployment of a [`ChangeSet`].
#[derive(Clone, Debug)]
pub enum ResourceAction {
    /// Add a new resource to the stack.
    Add,

    /// Modify an exsting resource in the stack.
    Modify,

    /// Remove a resource from the stack.
    Remove,
}

impl fmt::Display for ResourceAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add => "Add",
            Self::Modify => "Modify",
            Self::Remove => "Remove",
        }
        .fmt(f)
    }
}

impl std::str::FromStr for ResourceAction {
    type Err = String;

    fn from_str(action: &str) -> std::result::Result<Self, Self::Err> {
        match action {
            "Add" => Ok(Self::Add),
            "Modify" => Ok(Self::Modify),
            "Remove" => Ok(Self::Remove),
            _ => Err(action.to_string()),
        }
    }
}

pub(crate) async fn for_create(
    client: &CloudFormationClient,
    input: DeployInput,
) -> Result<ChangeSet> {
    create_change_set(client, input, ChangeSetType::Create).await
}

pub(crate) async fn for_update(
    client: &CloudFormationClient,
    input: DeployInput,
) -> Result<ChangeSet> {
    create_change_set(client, input, ChangeSetType::Update).await
}

pub(crate) async fn for_delete(
    client: &CloudFormationClient,
    stack_id: String,
    stack_name: String,
) -> Result<ChangeSet> {
    // Since this isn't a real change set, we use DescribeStackResources to generate the changes.
    let request = DescribeStackResourcesInput {
        stack_name: Some(stack_id.clone()),
        ..DescribeStackResourcesInput::default()
    };
    let resource_changes = client
        .describe_stack_resources(request)
        .await?
        .stack_resources
        .expect("DescribeStackResources without stack_resources")
        .into_iter()
        .map(|resource| ResourceChange {
            action: ResourceAction::Remove,
            logical_resource_id: resource.logical_resource_id,
            physical_resource_id: resource.physical_resource_id,
            resource_type: resource.resource_type,
        })
        .collect();
    Ok(ChangeSet {
        effect: Effect::Delete,
        stack_id,
        stack_name,
        resource_changes,
    })
}

pub(crate) fn execute(
    client: &CloudFormationClient,
    change_set: ChangeSet,
    mut on_complete: Option<oneshot::Sender<()>>,
) -> impl Stream<Item = Result<StackEvent>> + '_ {
    try_stream! {
        let since = Utc::now();
        match &change_set.effect {
            Effect::Skip { .. } => {
                if let Some(event) = last_stack_event(client, &change_set.stack_id).await? {
                    yield event;
                }
                return
            },
            Effect::Create { id } | Effect::Update { id } => {
                let request = ExecuteChangeSetInput {
                    change_set_name: id.to_string(),
                    ..ExecuteChangeSetInput::default()
                };
                client.execute_change_set(request).await?;
            },
            Effect::Delete => {
                let request = DeleteStackInput {
                    stack_name: change_set.stack_id.clone(),
                    ..DeleteStackInput::default()
                };
                client.delete_stack(request).await?;
            }
        }

        let stack_events = stack_events_since(client, &change_set.stack_id, &since);
        pin_mut!(stack_events);

        let mut stack_error_event = None;
        let mut resource_error_events: Option<Vec<_>> = None;
        while let Some(event) = stack_events.try_next().await? {
            if event.resource_status.is_error() {
                if event.physical_resource_id.as_deref() == Some(&change_set.stack_id) {
                    stack_error_event = Some(event.clone());
                } else {
                    resource_error_events.get_or_insert_with(Default::default).push(event.clone());
                }
            }
            yield event;
        }

        if let Some(stack_error_event) = stack_error_event {
            Err(Error::ExecuteChangeSetFailed {
                change_set,
                stack_error_event,
                resource_error_events: resource_error_events.unwrap_or_default(),
            })?;
        }

        if let Some(on_complete) = on_complete.take() {
            on_complete.send(()).ok();
        }
    }
}

enum ChangeSetType {
    Create,
    Update,
}

async fn create_change_set(
    client: &CloudFormationClient,
    input: DeployInput,
    change_set_type: ChangeSetType,
) -> Result<ChangeSet> {
    let request = CreateChangeSetInput {
        change_set_name: format!("cfn-deploy-{}", Utc::now().timestamp()),
        change_set_type: Some(match change_set_type {
            ChangeSetType::Create => "CREATE".to_string(),
            ChangeSetType::Update => "UPDATE".to_string(),
        }),
        parameters: Some(
            input
                .parameters
                .into_iter()
                .map(|(key, value)| Parameter {
                    parameter_key: Some(key),
                    parameter_value: Some(value),
                    ..Parameter::default()
                })
                .collect(),
        ),
        stack_name: input.stack_name.clone(),
        template_body: Some(input.template_body),
        ..CreateChangeSetInput::default()
    };
    let output = client.create_change_set(request).await?;
    let change_set_id = output.id.expect("CreateChangeSetOutput without id");
    let stack_id = output
        .stack_id
        .expect("CreateChangeSetOutput without stack_id");

    // Wait for the change set to become available.
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let request = DescribeChangeSetInput {
        change_set_name: change_set_id.clone(),
        ..DescribeChangeSetInput::default()
    };
    let change_set = loop {
        interval.tick().await;
        let change_set = client.describe_change_set(request.clone()).await?;
        if !matches!(
            change_set.status.as_deref(),
            Some("CREATE_PENDING") | Some("CREATE_IN_PROGRESS")
        ) {
            break change_set;
        }
    };

    if change_set.execution_status.as_deref() != Some("AVAILABLE") {
        let status = change_set.status.expect("DescribeChangeSet without status");
        let status_reason = change_set.status_reason;
        // We want to allow empty change sets to ensure idempotency.
        if status == "FAILED"
            && status_reason
                .as_deref()
                .unwrap_or_default()
                .contains("The submitted information didn't contain changes.")
        {
            return Ok(ChangeSet {
                effect: Effect::Skip,
                stack_id,
                stack_name: input.stack_name,
                resource_changes: Vec::new(),
            });
        }

        return Err(Error::CreateChangeSetFailed {
            status,
            status_reason,
        });
    }

    let effect = match change_set_type {
        ChangeSetType::Create => Effect::Create { id: change_set_id },
        ChangeSetType::Update => Effect::Update { id: change_set_id },
    };
    let resource_changes = change_set
        .changes
        .expect("DescribeChangeSet without changes")
        .into_iter()
        .map(ResourceChange::from_native)
        .collect();
    Ok(ChangeSet {
        effect,
        stack_id,
        stack_name: input.stack_name,
        resource_changes,
    })
}
