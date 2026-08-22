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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_schema_is_valid_json_object() {
        let value: Value = serde_json::from_str(AGENT_SCHEMA_JSON)
            .expect("AGENT_SCHEMA_JSON should parse as JSON");
        assert!(value.is_object());
        assert!(value.get("schema_version").is_some());
    }

    #[test]
    fn embedded_schema_pretty_roundtrips() {
        let value: Value = serde_json::from_str(AGENT_SCHEMA_JSON).unwrap();
        let pretty = serde_json::to_string_pretty(&value).unwrap();
        assert!(pretty.starts_with('{'));
        let reparsed: Value = serde_json::from_str(&pretty).unwrap();
        assert_eq!(value, reparsed);
    }

    #[test]
    fn cmd_schema_json_mode_ok() {
        cmd_schema(true).unwrap();
    }

    #[test]
    fn cmd_schema_text_mode_ok() {
        cmd_schema(false).unwrap();
    }
}
