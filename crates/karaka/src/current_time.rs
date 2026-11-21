//! `current_time`, the one tool `karaka chat` has: a stand-in that shows the
//! loop running a tool end to end, until real tools arrive.

use std::time::SystemTime;

use karaka_model::ToolSchema;
use karaka_tools::{Tool, ToolFuture, ToolOutput, ToolRun};
use serde_json::Value;

/// Tells the model the current time in UTC, such as
/// `Sat, 21 Nov 2026 09:30:00 GMT`. That is the HTTP date format, written by
/// `httpdate`, which the workspace already has for reading `Retry-After`.
pub struct CurrentTime;

impl Tool for CurrentTime {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "current_time".into(),
            description: "Returns the current date and time in UTC.".into(),
            parameters: r#"{"type":"object","properties":{}}"#.into(),
        }
    }

    fn execute<'a>(&'a self, _arguments: Value, _run: ToolRun) -> ToolFuture<'a> {
        Box::pin(async { ToolOutput::ok(httpdate::fmt_http_date(SystemTime::now())) })
    }
}

#[cfg(test)]
mod tests {
    use karaka_tools::{Tool, ToolRun};
    use serde_json::json;

    use super::CurrentTime;

    #[tokio::test]
    async fn it_answers_with_the_time_in_utc() {
        let run = ToolRun {
            call_id: "call_1".into(),
            name: "current_time".into(),
        };
        let output = CurrentTime.execute(json!({}), run).await;
        assert!(!output.is_error);
        assert!(output.content.ends_with(" GMT"), "{}", output.content);
        assert!(httpdate::parse_http_date(&output.content).is_ok());
    }
}
