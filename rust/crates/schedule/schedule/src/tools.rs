//! Agent-scoped Schedule management tools over the durable session fold.

use crate::domain::{
    allocate_schedule_id, create_after_schedule_record, create_at_schedule_record,
    create_every_schedule_record, fold_schedule_events, schedule_view, FoldedSchedules,
    ScheduleChange, ScheduleInputError, ScheduleLogError, ScheduleRecord,
    MIN_EVERY_INTERVAL_SECONDS,
};
use crate::persistence::flush_schedule_persistence;
use crate::runtime::Owner;
use async_trait::async_trait;
use dsh_agent::Agent;
use dsh_cordis::Context;
use dsh_session::{SessionEventData, SessionHeader};
use dsh_tools::{
    GenericCallView, Tool, ToolCall, ToolCallKind, ToolCallView, ToolError, ToolOutcome,
    ToolRuntime,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const CREATE_DESCRIPTION: &str = concat!(
    "Create one reminder in the current session. Supply a non-empty prompt and exactly one selector: ",
    "a positive safe-integer after_seconds delay, at as a strict offset date-time or local ",
    "date/time object, or safe-integer every_seconds of at least 300. ",
    "Fixed-rate reminders stay creation-aligned, skip missed occurrences, and batch one latest ",
    "occurrence per overdue rule. ",
    "Delivery is session-local: the reminder runs on time only while this session ",
    "is live and otherwise becomes overdue until the session is resumed.",
);

const LIST_DESCRIPTION: &str = concat!(
    "List every active reminder in the current session in creation order, including its exact id, ",
    "UTC target, scheduled or overdue state, and session-local delivery mode.",
);

const DELETE_DESCRIPTION: &str = concat!(
    "Delete one active reminder in the current session by the exact id returned by schedule_create ",
    "or schedule_list. Unknown or already-finished ids return deleted false.",
);

/// Register the three shared Schedule tools once. Visibility is scoped through
/// [`Tool::enabled_for`] against the live owner map.
pub(crate) fn register_shared_tools(
    ctx: &Context,
    owners: Arc<Mutex<HashMap<String, Owner>>>,
) -> dsh_cordis::Result<()> {
    let tools = ctx.service::<ToolRuntime>()?;
    tools.register(
        ctx,
        Arc::new(ScheduleCreateTool {
            ctx: ctx.clone(),
            owners: Arc::clone(&owners),
        }),
    )?;
    tools.register(
        ctx,
        Arc::new(ScheduleListTool {
            ctx: ctx.clone(),
            owners: Arc::clone(&owners),
        }),
    )?;
    tools.register(
        ctx,
        Arc::new(ScheduleDeleteTool {
            ctx: ctx.clone(),
            owners,
        }),
    )?;
    Ok(())
}

/// Bind one exact live root into the owner map so the shared tools accept it.
pub fn register_schedule_tools(
    owners: &Mutex<HashMap<String, Owner>>,
    owner: Owner,
) -> Result<(), String> {
    owners
        .lock()
        .map_err(|_| "schedule owners lock poisoned".to_string())?
        .insert(owner.agent.id().as_str().to_string(), owner);
    Ok(())
}

fn seed_length(header: &SessionHeader) -> i64 {
    header.seed_length.unwrap_or(0) as i64
}

fn json_text(value: &Value) -> ToolOutcome {
    ToolOutcome::text(value.to_string())
}

fn internal_error() -> Value {
    json!({
        "code": "internal_error",
        "message": "The schedule operation failed.",
    })
}

fn corrupt_log_error() -> Value {
    json!({
        "code": "corrupt_schedule_log",
        "message": "The session schedule log is corrupt.",
    })
}

fn persistence_error(operation: &str, id: Option<&str>) -> Value {
    let mut value = json!({
        "code": "persistence_uncertain",
        "message": "Schedule persistence is uncertain; retry with schedule_list before relying on this result.",
        "operation": operation,
    });
    if let Some(id) = id {
        value["id"] = json!(id);
    }
    value
}

fn input_error(error: &ScheduleInputError) -> Value {
    json!({
        "code": error.code,
        "message": error.message,
    })
}

fn view_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "string" },
                    "prompt": { "type": "string" },
                    "scheduledAt": { "type": "string" },
                    "state": { "type": "string", "enum": ["scheduled", "overdue"] },
                    "deliveryMode": { "type": "string", "const": "session-local" },
                    "kind": { "type": "string", "const": "after" },
                    "afterSeconds": { "type": "integer" },
                },
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "string" },
                    "prompt": { "type": "string" },
                    "scheduledAt": { "type": "string" },
                    "state": { "type": "string", "enum": ["scheduled", "overdue"] },
                    "deliveryMode": { "type": "string", "const": "session-local" },
                    "kind": { "type": "string", "const": "at" },
                },
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "string" },
                    "prompt": { "type": "string" },
                    "scheduledAt": { "type": "string" },
                    "state": { "type": "string", "enum": ["scheduled", "overdue"] },
                    "deliveryMode": { "type": "string", "const": "session-local" },
                    "kind": { "type": "string", "const": "every" },
                    "everySeconds": { "type": "integer" },
                },
            },
        ]
    })
}

fn basic_error_schema(code: &str) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "code": { "type": "string", "const": code },
            "message": { "type": "string" },
        },
    })
}

fn persistence_error_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "code": { "type": "string", "const": "persistence_uncertain" },
            "message": { "type": "string" },
            "operation": { "type": "string", "enum": ["create", "list", "delete"] },
            "id": { "type": "string" },
        },
    })
}

fn error_schemas() -> Vec<Value> {
    [
        "invalid_prompt",
        "invalid_selector",
        "invalid_rule",
        "invalid_time_zone",
        "not_future",
        "time_out_of_range",
        "frequency_too_high",
        "corrupt_schedule_log",
        "internal_error",
    ]
    .into_iter()
    .map(basic_error_schema)
    .chain(std::iter::once(persistence_error_schema()))
    .collect()
}

fn owner_for(owners: &Mutex<HashMap<String, Owner>>, call: &ToolCall) -> Option<Owner> {
    let id = call.agent_id.as_deref()?;
    owners
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(id)
        .cloned()
}

fn enabled(owners: &Mutex<HashMap<String, Owner>>, agent_id: Option<&str>) -> bool {
    agent_id.is_some_and(|id| {
        owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(id)
    })
}

fn notify_durable_change(owner: &Owner) {
    owner.runtime.request_drive();
}

async fn preflight(
    ctx: &Context,
    agent: &dyn Agent,
    operation: &str,
    id: Option<&str>,
) -> Option<Value> {
    match flush_schedule_persistence(ctx, agent.session().as_ref()).await {
        Ok(()) => None,
        Err(_) => Some(persistence_error(operation, id)),
    }
}

fn fold_for_tool(agent: &dyn Agent) -> Result<FoldedSchedules, Value> {
    match fold_schedule_events(
        &agent.session().events(),
        seed_length(agent.session().header()),
    ) {
        Ok(folded) => Ok(folded),
        Err(ScheduleLogError(_)) => Err(corrupt_log_error()),
    }
}

fn validate_create_args(args: &Value) -> Option<Value> {
    let Some(object) = args.as_object() else {
        return Some(json!({
            "code": "invalid_selector",
            "message": "schedule_create accepts exactly one of after_seconds, at, or every_seconds.",
        }));
    };
    if object.keys().any(|key| {
        key != "prompt" && key != "after_seconds" && key != "at" && key != "every_seconds"
    }) {
        return Some(json!({
            "code": "invalid_selector",
            "message": "schedule_create accepts exactly one of after_seconds, at, or every_seconds.",
        }));
    }
    let selectors = usize::from(object.contains_key("after_seconds"))
        + usize::from(object.contains_key("at"))
        + usize::from(object.contains_key("every_seconds"));
    if selectors != 1 {
        return Some(json!({
            "code": "invalid_selector",
            "message": "schedule_create accepts exactly one of after_seconds, at, or every_seconds.",
        }));
    }
    let prompt = object.get("prompt").and_then(Value::as_str).unwrap_or("");
    if prompt.trim().is_empty() {
        return Some(json!({
            "code": "invalid_prompt",
            "message": "prompt must be non-empty after trimming.",
        }));
    }
    if let Some(after) = object.get("after_seconds") {
        let Some(value) = json_safe_int(after) else {
            return Some(json!({
                "code": "invalid_rule",
                "message": "after_seconds must be a positive safe integer.",
            }));
        };
        if value <= 0 {
            return Some(json!({
                "code": "invalid_rule",
                "message": "after_seconds must be a positive safe integer.",
            }));
        }
    }
    if let Some(every) = object.get("every_seconds") {
        let Some(value) = json_safe_int(every) else {
            return Some(json!({
                "code": "invalid_rule",
                "message": "every_seconds must be a safe integer.",
            }));
        };
        if value < MIN_EVERY_INTERVAL_SECONDS {
            return Some(json!({
                "code": "frequency_too_high",
                "message": format!("every_seconds must be at least {MIN_EVERY_INTERVAL_SECONDS}."),
            }));
        }
    }
    None
}

fn json_safe_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                if (int as f64).fract() == 0.0 && (int as f64).abs() <= 9_007_199_254_740_991.0 {
                    return Some(int);
                }
            }
            if let Some(float) = number.as_f64() {
                if float.is_finite()
                    && float.fract() == 0.0
                    && float.abs() <= 9_007_199_254_740_991.0
                {
                    return Some(float as i64);
                }
            }
            None
        }
        _ => None,
    }
}

fn present(title: &str, kind: ToolCallKind, raw_input: Option<Value>) -> ToolCallView {
    ToolCallView::Generic(GenericCallView {
        title: title.to_string(),
        kind: Some(kind),
        raw_input,
        content: None,
    })
}

struct ScheduleCreateTool {
    ctx: Context,
    owners: Arc<Mutex<HashMap<String, Owner>>>,
}

#[async_trait]
impl Tool for ScheduleCreateTool {
    fn name(&self) -> &str {
        "schedule_create"
    }

    fn description(&self) -> &str {
        CREATE_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Reminder content to present when the target becomes due.",
                },
                "after_seconds": {
                    "type": "number",
                    "description": "Positive safe-integer delay in seconds.",
                },
                "every_seconds": {
                    "type": "number",
                    "description": format!(
                        "Fixed-rate safe-integer interval in seconds, at least {MIN_EVERY_INTERVAL_SECONDS}."
                    ),
                },
                "at": {
                    "description": "Absolute target as strict offset RFC 3339 or local date/time with an explicit IANA zone.",
                    "oneOf": [
                        { "type": "string" },
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "date": { "type": "string" },
                                "time": { "type": "string" },
                                "time_zone": { "type": "string" },
                            },
                            "required": ["date", "time", "time_zone"],
                        },
                    ],
                },
            },
            "required": ["prompt"],
        })
    }

    fn output_schema(&self) -> Option<Value> {
        let mut schemas = vec![view_schema()];
        schemas.extend(error_schemas());
        Some(json!({ "oneOf": schemas }))
    }

    fn enabled_for(&self, agent_id: Option<&str>) -> bool {
        enabled(&self.owners, agent_id)
    }

    fn present_call(&self, args: &Value) -> Option<ToolCallView> {
        Some(present(
            "Create reminder",
            ToolCallKind::Other,
            args.get("prompt").cloned(),
        ))
    }

    async fn execute(&self, _args: Value) -> Result<ToolOutcome, ToolError> {
        Ok(json_text(&internal_error()))
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let Some(owner) = owner_for(&self.owners, call) else {
            return Ok(json_text(&internal_error()));
        };
        if let Some(invalid) = validate_create_args(&call.args) {
            return Ok(json_text(&invalid));
        }
        let prompt = call
            .args
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let at = call.args.get("at").cloned();
        let after_seconds = call.args.get("after_seconds").and_then(json_safe_int);
        let every_seconds = call.args.get("every_seconds").and_then(json_safe_int);
        let ctx = self.ctx.clone();
        let value = owner
            .runtime
            .transactions()
            .run(owner.agent.as_ref(), || {
                let owner = owner.clone();
                let ctx = ctx.clone();
                let prompt = prompt.clone();
                async move {
                    if let Some(error) = preflight(&ctx, owner.agent.as_ref(), "create", None).await {
                        return error;
                    }
                    notify_durable_change(&owner);
                    let folded = match fold_for_tool(owner.agent.as_ref()) {
                        Ok(folded) => folded,
                        Err(error) => return error,
                    };
                    let id = allocate_schedule_id(&folded);
                    let now = chrono::Utc::now().timestamp_millis() as f64;
                    let record = if let Some(at) = at.as_ref() {
                        create_at_schedule_record(&id, &prompt, at, now).map(ScheduleRecord::At)
                    } else if let Some(after) = after_seconds {
                        create_after_schedule_record(&id, &prompt, after as f64, now)
                            .map(ScheduleRecord::After)
                    } else if let Some(every) = every_seconds {
                        create_every_schedule_record(&id, &prompt, every as f64, now)
                            .map(ScheduleRecord::Every)
                    } else {
                        return json!({
                            "code": "invalid_selector",
                            "message": "schedule_create accepts exactly one of after_seconds, at, or every_seconds.",
                        });
                    };
                    let record = match record {
                        Ok(record) => record,
                        Err(error) => return input_error(&error),
                    };
                    if owner
                        .agent
                        .session()
                        .append(
                            SessionEventData::Extension {
                                type_name: "schedule/change".into(),
                                data: ScheduleChange::Create {
                                    schedule: record.clone(),
                                }
                                .to_json(),
                            },
                            None,
                        )
                        .is_err()
                    {
                        return internal_error();
                    }
                    if let Some(error) =
                        preflight(&ctx, owner.agent.as_ref(), "create", Some(&id)).await
                    {
                        return error;
                    }
                    notify_durable_change(&owner);
                    schedule_view(&record, chrono::Utc::now().timestamp_millis()).to_json()
                }
            })
            .await;
        Ok(json_text(&value))
    }
}

struct ScheduleListTool {
    ctx: Context,
    owners: Arc<Mutex<HashMap<String, Owner>>>,
}

#[async_trait]
impl Tool for ScheduleListTool {
    fn name(&self) -> &str {
        "schedule_list"
    }

    fn description(&self) -> &str {
        LIST_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
        })
    }

    fn output_schema(&self) -> Option<Value> {
        let mut schemas = vec![json!({ "type": "array", "items": view_schema() })];
        schemas.extend(error_schemas());
        Some(json!({ "oneOf": schemas }))
    }

    fn enabled_for(&self, agent_id: Option<&str>) -> bool {
        enabled(&self.owners, agent_id)
    }

    fn present_call(&self, _args: &Value) -> Option<ToolCallView> {
        Some(present("List reminders", ToolCallKind::Read, None))
    }

    async fn execute(&self, _args: Value) -> Result<ToolOutcome, ToolError> {
        Ok(json_text(&internal_error()))
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let Some(owner) = owner_for(&self.owners, call) else {
            return Ok(json_text(&internal_error()));
        };
        let ctx = self.ctx.clone();
        let value = owner
            .runtime
            .transactions()
            .run(owner.agent.as_ref(), || {
                let owner = owner.clone();
                let ctx = ctx.clone();
                async move {
                    if let Some(error) = preflight(&ctx, owner.agent.as_ref(), "list", None).await {
                        return error;
                    }
                    notify_durable_change(&owner);
                    let folded = match fold_for_tool(owner.agent.as_ref()) {
                        Ok(folded) => folded,
                        Err(error) => return error,
                    };
                    let now = chrono::Utc::now().timestamp_millis();
                    Value::Array(
                        folded
                            .active
                            .iter()
                            .map(|record| schedule_view(record, now).to_json())
                            .collect(),
                    )
                }
            })
            .await;
        Ok(json_text(&value))
    }
}

struct ScheduleDeleteTool {
    ctx: Context,
    owners: Arc<Mutex<HashMap<String, Owner>>>,
}

#[async_trait]
impl Tool for ScheduleDeleteTool {
    fn name(&self) -> &str {
        "schedule_delete"
    }

    fn description(&self) -> &str {
        DELETE_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Exact session-local schedule id.",
                },
            },
        })
    }

    fn output_schema(&self) -> Option<Value> {
        let mut schemas = vec![
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "string" },
                    "deleted": { "type": "boolean", "const": true },
                },
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "string" },
                    "deleted": { "type": "boolean", "const": false },
                    "code": { "type": "string", "const": "schedule_not_found" },
                },
            }),
        ];
        schemas.extend(error_schemas());
        Some(json!({ "oneOf": schemas }))
    }

    fn enabled_for(&self, agent_id: Option<&str>) -> bool {
        enabled(&self.owners, agent_id)
    }

    fn present_call(&self, args: &Value) -> Option<ToolCallView> {
        Some(present(
            "Delete reminder",
            ToolCallKind::Other,
            args.get("id").cloned(),
        ))
    }

    async fn execute(&self, _args: Value) -> Result<ToolOutcome, ToolError> {
        Ok(json_text(&internal_error()))
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<ToolOutcome, ToolError> {
        let Some(owner) = owner_for(&self.owners, call) else {
            return Ok(json_text(&internal_error()));
        };
        let id = match call.args.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && id.trim() == id => id.to_string(),
            _ => {
                return Ok(json_text(&json!({
                    "code": "invalid_rule",
                    "message": "schedule_delete id must be non-empty without surrounding whitespace.",
                })));
            }
        };
        let ctx = self.ctx.clone();
        let value = owner
            .runtime
            .transactions()
            .run(owner.agent.as_ref(), || {
                let owner = owner.clone();
                let ctx = ctx.clone();
                let id = id.clone();
                async move {
                    if let Some(error) =
                        preflight(&ctx, owner.agent.as_ref(), "delete", Some(&id)).await
                    {
                        return error;
                    }
                    notify_durable_change(&owner);
                    let folded = match fold_for_tool(owner.agent.as_ref()) {
                        Ok(folded) => folded,
                        Err(error) => return error,
                    };
                    if !folded.active.iter().any(|record| record.id() == id) {
                        return json!({
                            "id": id,
                            "deleted": false,
                            "code": "schedule_not_found",
                        });
                    }
                    if owner
                        .agent
                        .session()
                        .append(
                            SessionEventData::Extension {
                                type_name: "schedule/change".into(),
                                data: ScheduleChange::Delete { id: id.clone() }.to_json(),
                            },
                            None,
                        )
                        .is_err()
                    {
                        return internal_error();
                    }
                    if let Some(error) =
                        preflight(&ctx, owner.agent.as_ref(), "delete", Some(&id)).await
                    {
                        return error;
                    }
                    notify_durable_change(&owner);
                    json!({ "id": id, "deleted": true })
                }
            })
            .await;
        Ok(json_text(&value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_args_require_one_selector_and_a_prompt() {
        assert_eq!(
            validate_create_args(&json!({ "prompt": "hi" })).unwrap()["code"],
            "invalid_selector"
        );
        assert_eq!(
            validate_create_args(&json!({
                "prompt": "hi",
                "after_seconds": 1,
                "every_seconds": 300
            }))
            .unwrap()["code"],
            "invalid_selector"
        );
        assert_eq!(
            validate_create_args(&json!({ "prompt": "   ", "after_seconds": 1 })).unwrap()["code"],
            "invalid_prompt"
        );
        assert!(validate_create_args(&json!({ "prompt": "hi", "after_seconds": 30 })).is_none());
        assert_eq!(
            validate_create_args(&json!({ "prompt": "hi", "every_seconds": 299 })).unwrap()["code"],
            "frequency_too_high"
        );
    }

    #[test]
    fn persistence_error_uses_the_typescript_sentence() {
        let value = persistence_error("create", Some("schedule-1"));
        assert_eq!(value["code"], "persistence_uncertain");
        assert_eq!(
            value["message"],
            "Schedule persistence is uncertain; retry with schedule_list before relying on this result."
        );
        assert_eq!(value["id"], "schedule-1");
    }
}
