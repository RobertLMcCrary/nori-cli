//! Defaults layered over the engine's normalized configuration without overriding user rules.

use std::ffi::OsStr;

use serde_json::Value;
use serde_json::json;

// OpenCode requires a finite JavaScript integer. Nori otherwise imposes no depth budget.
const ENGINE_DEPTH_CEILING: i64 = 9_007_199_254_740_991;

pub(crate) fn overlay(
    entries: &Value,
    inline: Option<&OsStr>,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let entries = entries
        .as_array()
        .ok_or("OpenCode config is not an entry list")?;
    let documents: Vec<_> = entries
        .iter()
        .filter_map(|entry| entry.get("info").and_then(Value::as_object))
        .collect();
    let experimental = documents
        .iter()
        .rev()
        .find_map(|doc| doc.get("experimental"));
    let has_depth = experimental
        .and_then(|value| value.get("subagent_depth"))
        .is_some();
    let has_subagent_rules = documents.iter().any(|doc| {
        doc.get("permissions")
            .and_then(Value::as_array)
            .is_some_and(|rules| {
                rules.iter().any(|rule| {
                    matches!(
                        rule.get("action").and_then(Value::as_str),
                        Some("subagent" | "*")
                    )
                })
            })
    });
    if has_depth && has_subagent_rules {
        return Ok(None);
    }
    let mut content: Value = match inline {
        Some(value) => {
            serde_json::from_str(value.to_str().ok_or("non-UTF-8 OpenCode inline config")?)?
        }
        None => json!({}),
    };
    let object = content
        .as_object_mut()
        .ok_or("OpenCode inline config must be an object")?;
    if !has_depth {
        let mut value = experimental.cloned().unwrap_or_else(|| json!({}));
        value
            .as_object_mut()
            .ok_or("invalid OpenCode experimental config")?
            .insert("subagent_depth".into(), json!(ENGINE_DEPTH_CEILING));
        object.insert("experimental".into(), value);
    }
    if !has_subagent_rules {
        let permissions = object.entry("permissions").or_insert_with(|| json!([]));
        permissions
            .as_array_mut()
            .ok_or("invalid OpenCode permissions config")?
            .push(json!({ "action": "subagent", "resource": "*", "effect": "allow" }));
    }
    Ok(Some(serde_json::to_string(&content)?))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::overlay;

    #[test]
    fn fills_missing_fields_and_preserves_latest_experimental_options() {
        let result = overlay(
            &json!([{ "info": { "experimental": { "todo": false } } }]),
            None,
        )
        .unwrap()
        .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result,
            json!({"experimental":{"todo":false,"subagent_depth":9007199254740991_i64},"permissions":[{"action":"subagent","resource":"*","effect":"allow"}]})
        );
    }

    #[test]
    fn respects_explicit_depth_and_agent_deny() {
        let result = overlay(&json!([{ "info": {"experimental":{"subagent_depth":1}, "agents":{"general":{"permissions":[{"action":"subagent","resource":"*","effect":"deny"}]}}} }]), None).unwrap().unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result,
            json!({"permissions":[{"action":"subagent","resource":"*","effect":"allow"}]})
        );
    }

    #[test]
    fn preserves_explicit_global_deny_and_inline_settings() {
        let result = overlay(
            &json!([{ "info": {"permissions":[{"action":"subagent","resource":"*","effect":"deny"}]}}]),
            Some(std::ffi::OsStr::new(r#"{"model":"local/coder"}"#)),
        ).unwrap().unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            result,
            json!({"model":"local/coder","experimental":{"subagent_depth":9007199254740991_i64}})
        );
    }
}
