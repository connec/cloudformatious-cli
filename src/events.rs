use std::{fmt, time::Duration};

use async_stream::try_stream;
use chrono::{DateTime, Utc};
use rusoto_cloudformation::{CloudFormation, CloudFormationClient, DescribeStackEventsInput};
use tokio_stream::Stream;

use crate::Result;

/// A stack event.
#[derive(Clone, Debug)]
pub struct StackEvent {
    /// The physical resource ID to which the event pertains.
    pub physical_resource_id: Option<String>,

    /// The logical resource ID to which the event pertains.
    pub logical_resource_id: String,

    /// The type of the resource to which the event pertains.
    pub resource_type: String,

    /// The status of the resource to which the event pertains.
    pub resource_status: ResourceStatus,

    /// A message with context for the status of the resource.
    pub resource_status_reason: Option<String>,

    /// The time at which the event occurred.
    pub timestamp: DateTime<Utc>,
}

impl StackEvent {
    fn from_native(event: rusoto_cloudformation::StackEvent) -> Self {
        Self {
            physical_resource_id: match event
                .physical_resource_id
                .expect("StackEvent without physical_resource_id")
            {
                physical_resource_id if physical_resource_id.is_empty() => None,
                physical_resource_id => Some(physical_resource_id),
            },
            logical_resource_id: event
                .logical_resource_id
                .expect("StackEvent without logical_resource_id"),
            resource_type: event
                .resource_type
                .expect("StackEvent without resource_type"),
            resource_status: event
                .resource_status
                .expect("StackEvent without resource_status")
                .parse()
                .expect("unknown ResourceStatus"),
            resource_status_reason: event.resource_status_reason,
            timestamp: DateTime::parse_from_rfc3339(&event.timestamp)
                .expect("StackEvent invalid timestamp")
                .into(),
        }
    }
}

/// A resource status.
///
/// This describes all possible statuses for CloudFormation stacks and their resources.
#[derive(Clone, Debug)]
pub enum ResourceStatus {
    ReviewInProgress,
    CreateInProgress,
    CreateFailed,
    CreateComplete,
    DeleteInProgress,
    DeleteFailed,
    DeleteComplete,
    RollbackInProgress,
    RollbackFailed,
    RollbackComplete,
}

impl ResourceStatus {
    /// Indicates whether the status is a terminal status.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::CreateFailed
            | Self::CreateComplete
            | Self::DeleteFailed
            | Self::DeleteComplete
            | Self::RollbackFailed
            | Self::RollbackComplete => true,

            // These are spelled out explicitly to ensure new variants are properly categorised.
            Self::ReviewInProgress
            | Self::CreateInProgress
            | Self::DeleteInProgress
            | Self::RollbackInProgress => false,
        }
    }

    /// Indicates whether the status indicates an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        match self {
            Self::CreateFailed
            | Self::DeleteFailed
            | Self::RollbackFailed
            | Self::RollbackComplete => true,
            Self::ReviewInProgress
            | Self::CreateInProgress
            | Self::CreateComplete
            | Self::DeleteInProgress
            | Self::DeleteComplete
            | Self::RollbackInProgress => false,
        }
    }
}

impl fmt::Display for ResourceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReviewInProgress => "REVIEW_IN_PROGRESS",
            Self::CreateInProgress => "CREATE_IN_PROGRESS",
            Self::CreateFailed => "CREATE_FAILED",
            Self::CreateComplete => "CREATE_COMPLETE",
            Self::DeleteInProgress => "DELETE_IN_PROGRESS",
            Self::DeleteFailed => "DELETE_FAILED",
            Self::DeleteComplete => "DELETE_COMPLETE",
            Self::RollbackInProgress => "ROLLBACK_IN_PROGRESS",
            Self::RollbackFailed => "ROLLBACK_FAILED",
            Self::RollbackComplete => "ROLLBACK_COMPLETE",
        }
        .fmt(f)
    }
}

impl std::str::FromStr for ResourceStatus {
    type Err = String;

    fn from_str(status: &str) -> std::result::Result<Self, Self::Err> {
        match status {
            "REVIEW_IN_PROGRESS" => Ok(Self::ReviewInProgress),
            "CREATE_IN_PROGRESS" => Ok(Self::CreateInProgress),
            "CREATE_FAILED" => Ok(Self::CreateFailed),
            "CREATE_COMPLETE" => Ok(Self::CreateComplete),
            "DELETE_IN_PROGRESS" => Ok(Self::DeleteInProgress),
            "DELETE_FAILED" => Ok(Self::DeleteFailed),
            "DELETE_COMPLETE" => Ok(Self::DeleteComplete),
            "ROLLBACK_IN_PROGRESS" => Ok(Self::RollbackInProgress),
            "ROLLBACK_FAILED" => Ok(Self::RollbackFailed),
            "ROLLBACK_COMPLETE" => Ok(Self::RollbackComplete),
            _ => Err(status.to_string()),
        }
    }
}

pub(crate) fn stack_events_since<'client>(
    client: &'client CloudFormationClient,
    stack_id: &str,
    since: &DateTime<Utc>,
) -> impl Stream<Item = Result<StackEvent>> + 'client {
    let stack_id = stack_id.to_string();
    let mut since = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    let request = DescribeStackEventsInput {
        stack_name: Some(stack_id.clone()),
        ..DescribeStackEventsInput::default()
    };

    try_stream! {
        loop {
            interval.tick().await;
            let mut events = client
                .describe_stack_events(request.clone())
                .await?
                .stack_events
                .expect("DescribeStackEvents without stack_events")
                .into_iter()
                .filter({
                    let since = since.clone();
                    move |event| event.timestamp > since
                })
                .map(StackEvent::from_native)
                .peekable();

            let mut is_terminal = false;
            if let Some(last_event) = events.peek() {
                is_terminal = last_event.physical_resource_id.as_deref() == Some(&stack_id) && last_event.resource_status.is_terminal();
                since = last_event.timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            }

            for event in events.rev() {
                yield event;
            }

            if is_terminal {
                break;
            }
        }
    }
}

pub(crate) async fn last_stack_event(
    client: &CloudFormationClient,
    stack_id: &str,
) -> Result<Option<StackEvent>> {
    let request = DescribeStackEventsInput {
        stack_name: Some(stack_id.to_string()),
        ..DescribeStackEventsInput::default()
    };
    Ok(client
        .describe_stack_events(request)
        .await?
        .stack_events
        .expect("DescribeStackEvents without stack_events")
        .into_iter()
        .map(StackEvent::from_native)
        .next())
}
