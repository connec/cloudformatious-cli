use std::fmt;

use cloudformatious::{status_reason::StatusReasonDetail, StackFailure, StackWarning};
use colored::Colorize;

const NO_REASON: &str = "No reason";

#[derive(Debug)]
pub enum Error {
    Warning(StackWarning),
    Failure(StackFailure),
    Other(Box<dyn std::error::Error>),
}

impl Error {
    pub fn other<E: Into<Box<dyn std::error::Error>>>(error: E) -> Self {
        Self::Other(error.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warning(warning) => warning.fmt(f),
            Self::Failure(failure) => {
                writeln!(f, "Failed to apply stack {}:\n", failure.stack_id.bold())?;

                let status = failure.stack_status.to_string();
                writeln!(f, "   {} {}", "Status:".bold(), status.red())?;
                writeln!(f, "   {} {}", "Reason:".bold(), failure.stack_status_reason)?;

                if let Some(hint) = failure.stack_status_reason().detail().and_then(get_hint) {
                    writeln!(f, "   {:<7} {}", "Hint:".bold(), hint)?;
                }

                if !failure.resource_events.is_empty() {
                    writeln!(f, "\nWhat went wrong? The following resource errors occurred during the operation:")?;
                    for (index, (resource_status, event_details)) in
                        failure.resource_events.iter().enumerate()
                    {
                        let resource = event_details.logical_resource_id();
                        let type_ = event_details.resource_type();
                        let status = resource_status.to_string();
                        let reason = event_details
                            .resource_status_reason()
                            .inner()
                            .unwrap_or(NO_REASON);
                        writeln!(f, "\n{}. {} {}", index + 1, "Resource:".bold(), resource)?;
                        writeln!(f, "   {:<9} {}", "Type:".bold(), type_)?;
                        writeln!(f, "   {:<9} {}", "Status:".bold(), status.red())?;
                        writeln!(f, "   {:<9} {}", "Reason:".bold(), reason)?;

                        if let Some(hint) = event_details
                            .resource_status_reason()
                            .detail()
                            .and_then(get_hint)
                        {
                            writeln!(f, "   {:<9} {}", "Hint:".bold(), hint)?;
                        }
                    }
                }

                Ok(())
            }
            Self::Other(error) => {
                write!(f, "{}", error)?;
                let chain = std::iter::successors(error.source(), |error| error.source());
                for error in chain {
                    write!(f, ": {}", error)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Warning(_) | Self::Failure(_) => None,
            Self::Other(error) => Some(error.as_ref()),
        }
    }
}

fn get_hint(detail: StatusReasonDetail) -> Option<String> {
    match detail {
        StatusReasonDetail::CreationCancelled => Some("See preceding resource errors".to_string()),
        StatusReasonDetail::MissingPermission(detail) => Some(format!(
            "Give {} the {} permission",
            detail
                .principal
                .map(Colorize::bold)
                .unwrap_or_else(|| "yourself".normal()),
            detail.permission.bold()
        )),
        StatusReasonDetail::ResourceErrors(detail) => Some(format!(
            "See resource error(s) for {}",
            display_list(detail.logical_resource_ids().map(|id| id.bold()))
        )),
        _ => None,
    }
}

fn display_list<I, T>(iter: I) -> impl fmt::Display
where
    I: IntoIterator<Item = T>,
    T: fmt::Display,
{
    use fmt::Write;

    let mut output = String::new();

    let mut iter = iter.into_iter().peekable();
    let mut seen = 0;
    while let Some(item) = iter.next() {
        if seen > 0 {
            let next = iter.peek();
            if seen > 1 || next.is_some() {
                output.push(',');
            }
            if next.is_none() {
                output.push_str(" and");
            }
            output.push(' ');
        }
        write!(&mut output, "{}", item).unwrap();
        seen += 1;
    }

    output
}

#[test]
fn test_display_list() {
    assert_eq!(&display_list(&[1]).to_string(), "1");

    assert_eq!(&display_list(&[1, 2]).to_string(), "1 and 2");

    assert_eq!(&display_list(&[1, 2, 3]).to_string(), "1, 2, and 3");

    assert_eq!(&display_list(&[1, 2, 3, 4]).to_string(), "1, 2, 3, and 4");
}
