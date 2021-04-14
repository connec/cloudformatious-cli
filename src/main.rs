use std::{convert::TryInto, error::Error, fmt, fs, io, time::Duration};

use indexmap::IndexMap;
use rusoto_cloudformation::CloudFormationClient;
use rusoto_core::{request::TlsError, HttpClient, Region};
use rusoto_credential::{
    AutoRefreshingProvider, ChainProvider, CredentialsError, ProvideAwsCredentials,
};
use termion::{event::Key, input::TermRead, raw::IntoRawMode, screen::AlternateScreen};
use tokio_stream::StreamExt;
use tui::{
    backend::TermionBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Span, Spans, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Terminal,
};

use cfn_deploy::{
    CloudFormationExt, DeployInput, ResourceAction, ResourceChange, ResourceStatus, StackEvent,
};

#[tokio::main]
async fn main() {
    let stderr = io::stderr().into_raw_mode().unwrap();
    let stderr = AlternateScreen::from(stderr);
    let backend = TermionBackend::new(stderr);
    let mut terminal = Terminal::new(backend).unwrap();

    {
        let raw_handle = io::stderr().into_raw_mode().unwrap();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            raw_handle.suspend_raw_mode().unwrap();
            default_hook(info);
        }));
    }

    match try_main(&mut terminal)
        .await
        .map_err(|error| error.downcast::<cfn_deploy::Error>().map(|error| *error))
    {
        Ok(_) => {
            let stdin = io::stdin();
            for key in stdin.keys() {
                if let Key::Char('q') = key.unwrap() {
                    break;
                }
            }
            drop(terminal);

            println!("Success!");
        }
        Err(Ok(cfn_deploy::Error::ExecuteChangeSetFailed {
            change_set,
            stack_error_event,
            resource_error_events,
        })) => {
            drop(terminal);

            eprintln!(
                "\x1b[31mError:\x1b[0m failed to {} stack {}\n",
                change_set.effect.to_string().to_lowercase(),
                change_set.stack_name
            );

            let stack_error_event = FormattedStackEvent::new_unwrapped(&stack_error_event);
            eprintln!("Stack error event:\n");
            eprintln!("- {}\n", stack_error_event);

            eprintln!("Resource error events:\n");
            for event in resource_error_events {
                let event = FormattedStackEvent::new_unwrapped(&event);
                eprintln!("- {}", event);
            }
        }
        Err(Ok(error)) => {
            drop(terminal);

            eprintln!("{}", error);
            std::process::exit(1);
        }
        Err(Err(error)) => {
            drop(terminal);

            eprintln!("{}", error);
            std::process::exit(1);
        }
    }
}

struct State {
    stack_name: String,
    stack_state: Option<StackState>,
    events: Vec<StackEvent>,
}

struct StackState {
    stack_id: String,
    last_event: Option<StackEvent>,
    resource_states: IndexMap<String, ResourceState>,
}

struct ResourceState {
    plan: ResourceChange,
    last_event: Option<StackEvent>,
}

impl ResourceState {
    fn to_row(&self, reason_width: u16) -> Row {
        let (resource_status, resource_status_reason, height) = self
            .last_event
            .as_ref()
            .map(|event| {
                let event = FormattedStackEvent::new_wrapped(event, reason_width);
                (
                    event.resource_status,
                    event.resource_status_reason,
                    event.height,
                )
            })
            .unwrap_or_else(|| {
                (
                    (Text::from(""), FormatColor::Default),
                    (Text::from(""), FormatColor::Default),
                    1,
                )
            });

        Row::new(vec![
            Cell::from(self.plan.action.to_string()).style(Style::default().fg(
                match self.plan.action {
                    ResourceAction::Add => Color::LightGreen,
                    ResourceAction::Modify => Color::LightYellow,
                    ResourceAction::Remove => Color::LightRed,
                },
            )),
            Cell::from(self.plan.resource_type.as_str()),
            Cell::from(self.plan.logical_resource_id.as_str()),
            Cell::from(resource_status.0).style(resource_status.1.style()),
            Cell::from(resource_status_reason.0).style(resource_status_reason.1.style()),
        ])
        .height(height)
    }
}

impl State {
    fn render<B: tui::backend::Backend>(&self, terminal: &mut Terminal<B>) -> io::Result<()> {
        terminal.draw(|f| {
            let wrapper = Block::default().borders(Borders::ALL);
            let inner_rect = wrapper.inner(f.size());
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(f.size());
            match &self.stack_state {
                None => {
                    let wrapper = wrapper.title(Spans::from(vec![
                        Span::raw(format!("cfn-deploy – {} – ", self.stack_name)),
                        Span::styled("PENDING", Style::default().fg(Color::Yellow)),
                    ]));
                    let content = Paragraph::new("Starting deployment...").block(wrapper);
                    f.render_widget(content, layout[0]);
                }
                Some(stack_state) => {
                    let wrapper = wrapper.title(Spans::from(vec![
                        Span::raw(format!("cfn-deploy – {} – ", self.stack_name)),
                        stack_state
                            .last_event
                            .as_ref()
                            .map(|event| {
                                Span::styled(
                                    event.resource_status.to_string(),
                                    colorize_status(&event.resource_status).style(),
                                )
                            })
                            .unwrap_or_else(|| {
                                Span::styled("PENDING", Style::default().fg(Color::Yellow))
                            }),
                    ]));
                    let table_constraints = [
                        Constraint::Length(6),
                        Constraint::Percentage(20),
                        Constraint::Percentage(20),
                        Constraint::Length(20),
                        Constraint::Min(0),
                    ];
                    let table_layout = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints(&table_constraints[..])
                        .split(inner_rect);
                    let reason_width = table_layout[4].width;
                    let table = Table::new(
                        stack_state
                            .resource_states
                            .values()
                            .map(|state| state.to_row(reason_width)),
                    )
                    .header(
                        Row::new(vec![
                            Cell::from("Action").style(Style::default().fg(Color::Gray)),
                            Cell::from("Resource type").style(Style::default().fg(Color::Gray)),
                            Cell::from("Resource").style(Style::default().fg(Color::Gray)),
                            Cell::from("Status").style(Style::default().fg(Color::Gray)),
                            Cell::from("Status reason").style(Style::default().fg(Color::Gray)),
                        ])
                        .bottom_margin(1),
                    )
                    .block(wrapper)
                    .widths(&table_constraints);
                    f.render_widget(table, layout[0]);
                }
            }

            let events = Block::default().borders(Borders::ALL).title("Events");
            let table_constraints = [
                Constraint::Length(29),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Length(20),
                Constraint::Min(0),
            ];
            let table_layout = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(&table_constraints[..])
                .split(inner_rect);
            let reason_width = table_layout[4].width;
            let table = Table::new(
                self.events
                    .iter()
                    .map(|event| FormattedStackEvent::new_wrapped(event, reason_width).into_row()),
            )
            .block(events)
            .widths(&table_constraints);
            f.render_stateful_widget(table, layout[1], &mut {
                let mut table_state = TableState::default();
                table_state.select(Some(self.events.len()));
                table_state
            });
        })
    }
}

enum FormatColor {
    Default,
    Red,
    Green,
    Yellow,
}

impl FormatColor {
    fn style(&self) -> Style {
        match self {
            Self::Default => Style::reset(),
            color => Style::default().fg(match color {
                Self::Default => unreachable!(),
                Self::Red => Color::Red,
                Self::Green => Color::Green,
                Self::Yellow => Color::Yellow,
            }),
        }
    }
}

impl fmt::Display for FormatColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\x1b[{}m",
            match self {
                Self::Default => 0,
                Self::Red => 31,
                Self::Green => 32,
                Self::Yellow => 33,
            }
        )
    }
}

struct FormattedStackEvent<'a> {
    timestamp: (Text<'a>, FormatColor),
    resource_type: (Text<'a>, FormatColor),
    logical_resource_id: (Text<'a>, FormatColor),
    resource_status: (Text<'a>, FormatColor),
    resource_status_reason: (Text<'a>, FormatColor),
    height: u16,
}

impl<'a> FormattedStackEvent<'a> {
    fn new_unwrapped(event: &'a StackEvent) -> Self {
        Self {
            timestamp: (
                Text::from(event.timestamp.to_rfc3339()),
                FormatColor::Default,
            ),
            resource_type: (
                Text::from(event.resource_type.as_str()),
                FormatColor::Default,
            ),
            logical_resource_id: (
                Text::from(event.logical_resource_id.as_str()),
                FormatColor::Default,
            ),
            resource_status: (
                Text::from(event.resource_status.to_string()),
                colorize_status(&event.resource_status),
            ),
            resource_status_reason: (
                Text::from(
                    event
                        .resource_status_reason
                        .as_deref()
                        .unwrap_or("Unknown reason (debug via AWS Console)"),
                ),
                FormatColor::Default,
            ),
            height: 1,
        }
    }

    fn new_wrapped(event: &'a StackEvent, wrap_width: u16) -> Self {
        let (resource_status_reason, height) = event
            .resource_status_reason
            .as_deref()
            .map(|reason| {
                let reason = Text::from(textwrap::fill(reason, usize::from(wrap_width)));
                let height = reason.height().try_into().unwrap();
                ((reason, FormatColor::Default), height)
            })
            .unwrap_or_else(|| ((Text::from(""), FormatColor::Default), 1));
        Self {
            timestamp: (
                Text::from(event.timestamp.to_rfc3339()),
                FormatColor::Default,
            ),
            resource_type: (
                Text::from(event.resource_type.as_str()),
                FormatColor::Default,
            ),
            logical_resource_id: (
                Text::from(event.logical_resource_id.as_str()),
                FormatColor::Default,
            ),
            resource_status: (
                Text::from(event.resource_status.to_string()),
                colorize_status(&event.resource_status),
            ),
            resource_status_reason,
            height,
        }
    }

    fn into_row(self) -> Row<'a> {
        Row::new(vec![
            Cell::from(self.timestamp.0).style(self.timestamp.1.style()),
            Cell::from(self.resource_type.0).style(self.resource_type.1.style()),
            Cell::from(self.logical_resource_id.0).style(self.logical_resource_id.1.style()),
            Cell::from(self.resource_status.0).style(self.resource_status.1.style()),
            Cell::from(self.resource_status_reason.0).style(self.resource_status_reason.1.style()),
        ])
        .height(self.height)
    }
}

impl<'a> fmt::Display for FormattedStackEvent<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}{} {}{}{} {}{}{} {}{}{} {}{}{}",
            self.timestamp.1,
            self.timestamp
                .0
                .lines
                .iter()
                .flat_map(|Spans(spans)| spans)
                .map(|span| span.content.to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            FormatColor::Default,
            self.resource_type.1,
            self.resource_type
                .0
                .lines
                .iter()
                .flat_map(|Spans(spans)| spans)
                .map(|span| span.content.to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            FormatColor::Default,
            self.logical_resource_id.1,
            self.logical_resource_id
                .0
                .lines
                .iter()
                .flat_map(|Spans(spans)| spans)
                .map(|span| span.content.to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            FormatColor::Default,
            self.resource_status.1,
            self.resource_status
                .0
                .lines
                .iter()
                .flat_map(|Spans(spans)| spans)
                .map(|span| span.content.to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            FormatColor::Default,
            self.resource_status_reason.1,
            self.resource_status_reason
                .0
                .lines
                .iter()
                .flat_map(|Spans(spans)| spans)
                .map(|span| span.content.to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            FormatColor::Default,
        )
    }
}

async fn try_main<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
) -> Result<(), Box<dyn Error>> {
    let credentials = get_credentials()?;
    let client = get_client(credentials, Region::EuWest2)?;

    let stack_name = "vpc".to_string();
    let parameters = Default::default();
    let template_body = fs::read_to_string("test/fixtures/vpc.yaml")?;

    let mut state = State {
        stack_name: stack_name.clone(),
        stack_state: None,
        events: Vec::new(),
    };
    state.render(terminal)?;

    let deploy = client.deploy(DeployInput {
        stack_name,
        parameters,
        template_body,
    });

    let mut change_sets = deploy.change_sets();
    while let Some(change_set) = change_sets.try_next().await? {
        {
            let change_set = change_set.change_set();

            let stack_state = state.stack_state.get_or_insert_with(|| StackState {
                stack_id: change_set.stack_id.clone(),
                last_event: None,
                resource_states: IndexMap::new(),
            });
            if stack_state.stack_id != change_set.stack_id {
                stack_state.stack_id = change_set.stack_id.clone();
            }

            for change in &change_set.resource_changes {
                stack_state.resource_states.insert(
                    change.logical_resource_id.clone(),
                    ResourceState {
                        plan: change.clone(),
                        last_event: None,
                    },
                );
            }
        }
        state.render(terminal)?;

        let mut events = change_set.events();
        while let Some(event) = events.try_next().await? {
            state.events.push(event.clone());

            // unwrap OK because we initialised above
            let stack_state = state.stack_state.as_mut().unwrap();

            if event.physical_resource_id.as_deref() == Some(&stack_state.stack_id) {
                stack_state.last_event = Some(event);
            } else {
                let resource_state = stack_state
                    .resource_states
                    .get_mut(&event.logical_resource_id)
                    .expect("event for unplanned resource");
                resource_state.last_event = Some(event);
            }

            state.render(terminal)?;
        }
    }

    Ok(())
}

fn get_credentials() -> Result<impl ProvideAwsCredentials + Send + Sync, CredentialsError> {
    let mut credentials = ChainProvider::new();
    credentials.set_timeout(Duration::from_secs(0));
    AutoRefreshingProvider::new(credentials)
}

fn get_client(
    credentials: impl ProvideAwsCredentials + Send + Sync + 'static,
    region: Region,
) -> Result<CloudFormationClient, TlsError> {
    let client = HttpClient::new()?;
    Ok(CloudFormationClient::new_with(client, credentials, region))
}

fn colorize_status(status: &ResourceStatus) -> FormatColor {
    match status {
        ResourceStatus::ReviewInProgress => FormatColor::Yellow,
        ResourceStatus::CreateInProgress => FormatColor::Yellow,
        ResourceStatus::CreateFailed => FormatColor::Red,
        ResourceStatus::CreateComplete => FormatColor::Green,
        ResourceStatus::DeleteInProgress => FormatColor::Yellow,
        ResourceStatus::DeleteFailed => FormatColor::Red,
        ResourceStatus::DeleteComplete => FormatColor::Green,
        ResourceStatus::RollbackInProgress => FormatColor::Yellow,
        ResourceStatus::RollbackFailed => FormatColor::Red,
        ResourceStatus::RollbackComplete => FormatColor::Red,
    }
}
