use rusoto_cloudformation::{
    CloudFormation, CloudFormationClient, DescribeStacksError, DescribeStacksInput,
    DescribeStacksOutput,
};
use rusoto_core::RusotoError;

use crate::Result;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StackStatus {
    NotFound,
    ExistsErr(String),
    ExistsOk,
}

impl StackStatus {
    pub(crate) async fn load(client: &CloudFormationClient, stack_name: &str) -> Result<Self> {
        let result = client
            .describe_stacks(DescribeStacksInput {
                stack_name: Some(stack_name.to_string()),
                ..DescribeStacksInput::default()
            })
            .await;
        Self::try_from_result(result)
    }

    // We could use `TryFrom` but we don't really want it in the API.
    fn try_from_result(
        result: std::result::Result<DescribeStacksOutput, RusotoError<DescribeStacksError>>,
    ) -> Result<Self> {
        match result {
            Ok(DescribeStacksOutput { stacks, .. }) => {
                let stack = stacks
                    .expect("DescribeStacks without stacks")
                    .into_iter()
                    .next()
                    .expect("DescribeStacks with empty result");
                match stack.stack_status.as_str() {
                    "REVIEW_IN_PROGRESS" => Ok(Self::NotFound),
                    "DELETE_FAILED" | "ROLLBACK_FAILED" | "ROLLBACK_COMPLETE" => {
                        Ok(Self::ExistsErr(stack.stack_id.expect("Stack without id")))
                    }
                    // TODO: be more selective here
                    _ => Ok(Self::ExistsOk),
                }
            }
            Err(RusotoError::Unknown(response))
                if response.status == http::StatusCode::BAD_REQUEST =>
            {
                Ok(Self::NotFound)
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::HeaderMap;
    use rusoto_cloudformation::Stack;
    use rusoto_core::request::BufferedHttpResponse;

    use super::*;

    #[test]
    fn stack_state_try_from() {
        let result = Ok(DescribeStacksOutput {
            stacks: Some(vec![Stack {
                stack_status: "REVIEW_IN_PROGRESS".to_string(),
                ..Stack::default()
            }]),
            ..DescribeStacksOutput::default()
        });
        assert_eq!(
            StackStatus::try_from_result(result).unwrap(),
            StackStatus::NotFound
        );

        let result = Ok(DescribeStacksOutput {
            stacks: Some(vec![Stack {
                stack_id: Some("<id>".to_string()),
                stack_status: "DELETE_FAILED".to_string(),
                ..Stack::default()
            }]),
            ..DescribeStacksOutput::default()
        });
        assert_eq!(
            StackStatus::try_from_result(result).unwrap(),
            StackStatus::ExistsErr("<id>".to_string())
        );

        let result = Ok(DescribeStacksOutput {
            stacks: Some(vec![Stack {
                stack_id: Some("<id>".to_string()),
                stack_status: "ROLLBACK_FAILED".to_string(),
                ..Stack::default()
            }]),
            ..DescribeStacksOutput::default()
        });
        assert_eq!(
            StackStatus::try_from_result(result).unwrap(),
            StackStatus::ExistsErr("<id>".to_string())
        );

        let result = Ok(DescribeStacksOutput {
            stacks: Some(vec![Stack {
                stack_id: Some("<id>".to_string()),
                stack_status: "ROLLBACK_COMPLETE".to_string(),
                ..Stack::default()
            }]),
            ..DescribeStacksOutput::default()
        });
        assert_eq!(
            StackStatus::try_from_result(result).unwrap(),
            StackStatus::ExistsErr("<id>".to_string())
        );

        let result = Ok(DescribeStacksOutput {
            stacks: Some(vec![Stack {
                stack_status: "ANYTHING_ELSE".to_string(),
                ..Stack::default()
            }]),
            ..DescribeStacksOutput::default()
        });
        assert_eq!(
            StackStatus::try_from_result(result).unwrap(),
            StackStatus::ExistsOk
        );

        let result = Err(RusotoError::Unknown(BufferedHttpResponse {
            status: http::StatusCode::BAD_REQUEST,
            body: Bytes::from(""),
            headers: HeaderMap::default(),
        }));
        assert_eq!(
            StackStatus::try_from_result(result).unwrap(),
            StackStatus::NotFound
        );
    }
}
