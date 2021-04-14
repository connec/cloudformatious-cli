//! AWS CloudFormation deployment library wrapping [`rusoto_cloudformation`].

#![warn(clippy::pedantic)]

mod change_set;
mod error;
mod events;
mod stack_status;

use std::{
    collections::BTreeMap, future::Future, hint::unreachable_unchecked, mem, pin::Pin, task,
};

use async_stream::try_stream;
use pin_utils::pin_mut;
use rusoto_cloudformation::{CloudFormation, CloudFormationClient};
use tokio::sync::oneshot;
use tokio_stream::{Stream, StreamExt};

use crate::stack_status::StackStatus;

pub use crate::{
    change_set::{ChangeSet, Effect, ResourceAction, ResourceChange},
    error::{Error, Result},
    events::{ResourceStatus, StackEvent},
};

/// CloudFormation extension trait with a high-level `deploy` API.
pub trait CloudFormationExt: CloudFormation {
    fn deploy(&self, _: DeployInput) -> Deploy<'_>;
}

impl CloudFormationExt for CloudFormationClient {
    /// Deploy a CloudFormation stack.
    ///
    /// Under the hood this orchestrates the following CloudFormation actions:
    ///
    /// - `DescribeStacks` to determine the current status of the stack.
    /// - If the stack is in `RollbackComplete` (e.g. failed to create):
    ///   - `DescribeStackResources` to build the deletion [`ChangeSet`].
    ///   - `DeleteStack` to... delete the stack.
    ///   - `DescribeStackEvents` to emit [`StackEvent`]s and determine when the operation has
    ///     completed.
    /// - `CreateChangeSet` to begin change set creation.
    /// - `DescribeChangeSet` to determine when the change set is available and build the
    ///   [`ChangeSet`].
    /// - `ExecuteChangeSet` to begin change set execution.
    /// - `DescribeStackEvents` to emit [`StackEvent`]s and determine when the deployment has
    ///   completed.
    ///
    /// The returned future encapsulates the entire deployment process. If you need more control or
    /// visibility of deployments see [`Deploy::change_sets`] and [`Deploy::events`].
    ///
    /// # Examples
    ///
    /// Wait for the entire deployment process to conclude:
    ///
    /// ```no_run
    /// use std::collections::BTreeMap;
    ///
    /// use rusoto_cloudformation::CloudFormationClient;
    /// use tokio_stream::StreamExt;
    ///
    /// use cfn_deploy::{CloudFormationExt, DeployInput};
    ///
    /// # async fn eg() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = CloudFormationClient::new(rusoto_core::Region::EuWest2);
    /// let deploy = client.deploy(DeployInput {
    ///     stack_name: "my-stack".to_string(),
    ///     parameters: BTreeMap::new(),
    ///     template_body: "...".to_string(),
    /// });
    ///
    /// deploy.await?;
    /// eprintln!("Deploy finished!");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Print the change sets and manually advance the deployment:
    ///
    /// ```no_run
    /// # use std::collections::BTreeMap;
    /// # use rusoto_cloudformation::CloudFormationClient;
    /// # use tokio_stream::StreamExt;
    /// # use cfn_deploy::{CloudFormationExt, DeployInput};
    /// # async fn eg() -> Result<(), Box<dyn std::error::Error>> {
    /// #     let client = CloudFormationClient::new(rusoto_core::Region::EuWest2);
    /// #     let deploy = client.deploy(DeployInput {
    /// #         stack_name: String::new(),
    /// #         parameters: BTreeMap::new(),
    /// #         template_body: String::new()
    /// #     });
    /// let mut change_sets = deploy.change_sets();
    /// while let Some(change_set) = change_sets.try_next().await? {
    ///     eprintln!("{:#?}", change_set.change_set());
    ///     change_set.await?;
    /// }
    /// eprintln!("Deploy finished!");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Print events as they occur during deployment:
    ///
    /// ```no_run
    /// # use std::collections::BTreeMap;
    /// # use rusoto_cloudformation::CloudFormationClient;
    /// # use tokio_stream::StreamExt;
    /// # use cfn_deploy::{CloudFormationExt, DeployInput};
    /// # async fn eg() -> Result<(), Box<dyn std::error::Error>> {
    /// #     let client = CloudFormationClient::new(rusoto_core::Region::EuWest2);
    /// #     let deploy = client.deploy(DeployInput {
    /// #         stack_name: String::new(),
    /// #         parameters: BTreeMap::new(),
    /// #         template_body: String::new()
    /// #     });
    /// let mut events = deploy.events();
    /// while let Some(event) = events.try_next().await? {
    ///     eprintln!("{:#?}", event);
    /// }
    /// eprintln!("Deploy finished!");
    /// # Ok(())
    /// # }
    /// ```
    fn deploy(&self, input: DeployInput) -> Deploy<'_> {
        let inner = Box::pin(try_stream! {
            match StackStatus::load(self, &input.stack_name).await? {
                StackStatus::NotFound => {
                    let change_set = create_stack(self, input).await?;
                    yield change_set;
                },
                StackStatus::ExistsErr(stack_id) => {
                    let (tx, mut rx) = oneshot::channel();
                    let change_set = delete_stack(
                        self,
                        stack_id,
                        input.stack_name.clone(),
                        tx
                    ).await?;
                    yield change_set;

                    if let Ok(_) = rx.try_recv() {
                        let change_set = create_stack(self, input).await?;
                        yield change_set;
                    }
                }
                StackStatus::ExistsOk => {
                    let change_set = update_stack(self, input).await?;
                    yield change_set;
                }
            }
        });
        Deploy {
            inner: DeployChangeSets {
                inner,
                polling: None,
            },
        }
    }
}

/// Input struct for [`CloudFormationExt::deploy`].
#[derive(Clone, Debug)]
pub struct DeployInput {
    /// The name of the stack to deploy.
    pub stack_name: String,

    /// The parameters to set when creating or update the stack.
    pub parameters: BTreeMap<String, String>,

    /// The template body.
    pub template_body: String,
}

/// Future returned from [`CloudFormationExt::deploy`].
pub struct Deploy<'client> {
    inner: DeployChangeSets<'client>,
}

impl<'client> Deploy<'client> {
    /// Convert the `Future` into a `Stream` of [`DeployChangeSet`]s.
    ///
    /// This allows the different change sets that will be applied to be inspected prior to
    /// execution, if desired. See [`DeployChangeSets`] for more information.
    #[must_use]
    pub fn change_sets(self) -> DeployChangeSets<'client> {
        self.inner
    }

    /// Convert the `Future` into a `Stream` of [`StackEvent`]s.
    ///
    /// This streams the events returned by `DescribeStackEvents` as the deployment progresses. The
    /// stream terminates when the deployment has concluded.
    #[must_use]
    pub fn events(mut self) -> DeployEvents<'client> {
        let inner = Box::pin(try_stream! {
            while let Some(change_set) = self.inner.try_next().await? {
                let mut events = change_set.events();
                while let Some(event) = events.try_next().await? {
                    yield event;
                }
            }
        });
        DeployEvents { inner }
    }
}

impl Future for Deploy<'_> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        loop {
            if let Some(polling) = self.inner.polling.as_mut() {
                pin_mut!(polling);
                match polling.poll(ctx) {
                    task::Poll::Pending => return task::Poll::Pending,
                    task::Poll::Ready(Err(error)) => return task::Poll::Ready(Err(error)),
                    task::Poll::Ready(Ok(_)) => {
                        self.inner.polling = None;
                    }
                }
            }

            match self.inner.inner.as_mut().poll_next(ctx) {
                task::Poll::Pending => return task::Poll::Pending,
                task::Poll::Ready(None) => return task::Poll::Ready(Ok(())),
                task::Poll::Ready(Some(Err(error))) => return task::Poll::Ready(Err(error)),
                task::Poll::Ready(Some(Ok(change_set))) => {
                    self.inner.polling = Some(change_set);
                }
            }
        }
    }
}

/// Stream returned from [`Deploy::change_sets`].
pub struct DeployChangeSets<'client> {
    inner: Pin<Box<dyn Stream<Item = Result<DeployChangeSet<'client>>> + 'client>>,
    polling: Option<DeployChangeSet<'client>>,
}

impl<'client> Stream for DeployChangeSets<'client> {
    type Item = Result<DeployChangeSet<'client>>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        ctx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(ctx)
    }
}

/// Future yielded from [`Deploy::change_sets`].
pub struct DeployChangeSet<'client> {
    client: &'client CloudFormationClient,
    change_set: ChangeSet,
    state: DeployChangeSetState<'client>,
}

impl<'client> DeployChangeSet<'client> {
    /// Transition to the executing state.
    fn execute(&mut self) -> &mut DeployEvents<'client> {
        self.state.execute(self.client, &self.change_set)
    }

    /// Get the change set.
    #[must_use]
    pub fn change_set(&self) -> &ChangeSet {
        &self.change_set
    }

    /// Turn this future into a `Stream` of [`StackEvent`]s.
    #[must_use]
    pub fn events(mut self) -> DeployEvents<'client> {
        self.execute();
        match self.state {
            DeployChangeSetState::Executing { inner } => inner,
            _ => {
                unsafe {
                    // `Self::execute` will always leave us in the `Executing` state.
                    unreachable_unchecked()
                }
            }
        }
    }
}

impl<'client> Future for DeployChangeSet<'client> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        let inner = self.execute();
        loop {
            match inner.inner.as_mut().poll_next(ctx) {
                task::Poll::Pending => return task::Poll::Pending,
                task::Poll::Ready(None) => return task::Poll::Ready(Ok(())),
                task::Poll::Ready(Some(Err(error))) => return task::Poll::Ready(Err(error)),
                task::Poll::Ready(Some(Ok(_))) => {}
            }
        }
    }
}

enum DeployChangeSetState<'client> {
    Available {
        on_complete: Option<oneshot::Sender<()>>,
    },
    Executing {
        inner: DeployEvents<'client>,
    },
}

impl<'client> DeployChangeSetState<'client> {
    fn execute(
        &mut self,
        client: &'client CloudFormationClient,
        change_set: &ChangeSet,
    ) -> &mut DeployEvents<'client> {
        if let Self::Executing { inner } = self {
            return inner;
        }

        // Temporarily replace self with an empty value so we can take ownership.
        let this = mem::take(self);

        let on_complete = match this {
            Self::Available { on_complete } => on_complete,
            _ => unreachable!(),
        };
        let inner = DeployEvents {
            inner: Box::pin(crate::change_set::execute(
                client,
                change_set.clone(),
                on_complete,
            )),
        };
        *self = Self::Executing { inner };

        match self {
            Self::Executing { inner } => inner,
            _ => unreachable!(),
        }
    }
}

impl Default for DeployChangeSetState<'_> {
    fn default() -> Self {
        Self::Available { on_complete: None }
    }
}

/// Stream returned from [`Deploy::events`].
pub struct DeployEvents<'client> {
    inner: Pin<Box<dyn Stream<Item = Result<StackEvent>> + 'client>>,
}

impl Stream for DeployEvents<'_> {
    type Item = Result<StackEvent>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        ctx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(ctx)
    }
}

async fn create_stack(
    client: &CloudFormationClient,
    input: DeployInput,
) -> Result<DeployChangeSet<'_>> {
    let change_set = change_set::for_create(client, input).await?;
    Ok(DeployChangeSet {
        client,
        change_set,
        state: DeployChangeSetState::Available { on_complete: None },
    })
}

async fn update_stack(
    client: &CloudFormationClient,
    input: DeployInput,
) -> Result<DeployChangeSet<'_>> {
    let change_set = change_set::for_update(client, input).await?;
    Ok(DeployChangeSet {
        client,
        change_set,
        state: DeployChangeSetState::Available { on_complete: None },
    })
}

async fn delete_stack(
    client: &CloudFormationClient,
    stack_id: String,
    stack_name: String,
    on_complete: oneshot::Sender<()>,
) -> Result<DeployChangeSet<'_>> {
    let change_set = change_set::for_delete(client, stack_id, stack_name).await?;
    Ok(DeployChangeSet {
        client,
        change_set,
        state: DeployChangeSetState::Available {
            on_complete: Some(on_complete),
        },
    })
}
