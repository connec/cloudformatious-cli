use std::fmt;

use rusoto_cloudformation::{
    CreateChangeSetError, DeleteStackError, DescribeChangeSetError, DescribeStackEventsError,
    DescribeStackResourcesError, DescribeStacksError, ExecuteChangeSetError,
};
use rusoto_core::RusotoError;

use crate::{ChangeSet, StackEvent};

/// Convenient alias for [`Result`]`<T, `[`Error`]`>`.
///
/// [`Result`]: std::result::Result
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur when deploying CloudFormation.
#[derive(Debug)]
pub enum Error {
    CreateChangeSetFailed {
        status: String,
        status_reason: Option<String>,
    },
    ExecuteChangeSetFailed {
        change_set: ChangeSet,
        stack_error_event: StackEvent,
        resource_error_events: Vec<StackEvent>,
    },
    Other(Box<dyn std::error::Error>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateChangeSetFailed {
                status,
                status_reason,
            } => format!(
                "Change set could not be created successfully. Status: {}. Reason: {}",
                status,
                status_reason
                    .as_deref()
                    .unwrap_or("no reason (debug via the AWS Console).")
                    .to_string()
            )
            .fmt(f),
            Self::ExecuteChangeSetFailed {
                change_set,
                stack_error_event,
                resource_error_events,
            } => {
                format!(
                    "{} stack {} failed with status {}\n\n",
                    change_set.effect, change_set.stack_name, stack_error_event.resource_status
                )
                .fmt(f)?;

                if resource_error_events.is_empty() {
                    "There were no resource error events. Debug via the AWS Console.\n".fmt(f)?;
                } else {
                    "The following resource error events occurred:\n\n".fmt(f)?;
                    for event in resource_error_events {
                        format!(
                            "- {} ({}) \u{2013} {}: {}",
                            event.logical_resource_id,
                            event.resource_type,
                            event.resource_status,
                            event
                                .resource_status_reason
                                .as_deref()
                                .unwrap_or("Unknown reason (debug via the AWS Console)")
                        )
                        .fmt(f)?;
                    }
                }

                Ok(())
            }
            Self::Other(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<RusotoError<CreateChangeSetError>> for Error {
    fn from(error: RusotoError<CreateChangeSetError>) -> Self {
        Self::Other(error.into())
    }
}

impl From<RusotoError<DeleteStackError>> for Error {
    fn from(error: RusotoError<DeleteStackError>) -> Self {
        Self::Other(error.into())
    }
}

impl From<RusotoError<DescribeChangeSetError>> for Error {
    fn from(error: RusotoError<DescribeChangeSetError>) -> Self {
        Self::Other(error.into())
    }
}

impl From<RusotoError<DescribeStackEventsError>> for Error {
    fn from(error: RusotoError<DescribeStackEventsError>) -> Self {
        Self::Other(error.into())
    }
}

impl From<RusotoError<DescribeStackResourcesError>> for Error {
    fn from(error: RusotoError<DescribeStackResourcesError>) -> Self {
        Self::Other(error.into())
    }
}

impl From<RusotoError<DescribeStacksError>> for Error {
    fn from(error: RusotoError<DescribeStacksError>) -> Self {
        Self::Other(error.into())
    }
}

impl From<RusotoError<ExecuteChangeSetError>> for Error {
    fn from(error: RusotoError<ExecuteChangeSetError>) -> Self {
        Self::Other(error.into())
    }
}
