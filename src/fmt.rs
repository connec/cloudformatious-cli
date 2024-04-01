use std::{borrow::Cow, iter};

use cloudformatious::{change_set::ChangeSet, StackEvent, StackStatus, StatusSentiment};
use colored::{ColoredString, Colorize};
use futures_util::{Stream, StreamExt};

const AWS_CLOUDFORMATION_STACK: &str = "AWS::CloudFormation::Stack";
const SHORT_UPDATE_COMPLETE_CLEANUP_IN_PROGRESS: &str = "UPDATE_CLEANUP_IN_PROGRESS";
const SHORT_UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS: &str = "ROLLBACK_CLEANUP_IN_PROGRESS";

pub struct Sizing {
    resource_status: usize,
    logical_resource_id: usize,
    resource_type: usize,
}

impl Sizing {
    pub fn new_for_change_set(change_set: &ChangeSet) -> Self {
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

pub async fn print_events(sizing: &Sizing, mut events: impl Stream<Item = StackEvent> + Unpin) {
    while let Some(event) = events.next().await {
        let logical_resource_id: Cow<'_, _> = if let Some(stack_alias) = event.stack_alias() {
            [stack_alias, event.logical_resource_id()].join("/").into()
        } else {
            event.logical_resource_id().into()
        };
        eprintln!(
            "{:?} {:resource_status_size$} {:logical_resource_id_size$} {:resource_type_size$} {}",
            event.timestamp(),
            colorize_status(&event),
            logical_resource_id,
            event.resource_type(),
            event.resource_status_reason().unwrap_or("").bright_black(),
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
