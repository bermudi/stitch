use crate::error::StitchError;
use crate::report;
use serde_json::Value;

/// The canonical agent schema, embedded at compile time so the binary is
/// self-describing without a filesystem read. Kept in sync with
/// `docs/agent-schema.json` by the `schema_consistency` integration test.
const AGENT_SCHEMA_JSON: &str = include_str!("../../docs/agent-schema.json");

pub(crate) fn cmd_schema(json: bool) -> Result<(), StitchError> {
    let schema: Value = serde_json::from_str(AGENT_SCHEMA_JSON).map_err(|e| {
        StitchError::internal(format!("docs/agent-schema.json is invalid JSON: {e}"))
    })?;

    if json {
        report::write("schema", schema, Vec::new());
        return Ok(());
    }

    // Text mode: pretty-print the schema document.
    let pretty = serde_json::to_string_pretty(&schema)
        .map_err(|e| StitchError::internal(format!("schema serialize: {e}")))?;
    println!("{pretty}");
    Ok(())
}
