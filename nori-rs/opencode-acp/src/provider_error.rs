//! Preserve provider error identifiers carried in OpenCode v2 HTTP error bodies.

use serde_json::Value;

/// Keep OpenCode's full error while exposing a structured provider response at `error`.
pub(crate) fn data(error: &Value) -> Value {
    let mut result = error.clone();
    let Some(body) = error
        .pointer("/response/body")
        .or_else(|| error.get("bodyString"))
        .and_then(Value::as_str)
    else {
        return result;
    };
    let Ok(body) = serde_json::from_str::<Value>(body) else {
        return result;
    };
    let provider = body
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(&body);
    if provider.is_object()
        && let Some(object) = result.as_object_mut()
    {
        object.entry("error").or_insert_with(|| provider.clone());
    }
    result
}

/// A specific provider error code can signal exhaustion before OpenCode finishes retrying.
pub(crate) fn is_exhausted(error: &Value) -> bool {
    let data = data(error);
    [Some(&data), data.get("error")]
        .into_iter()
        .flatten()
        .any(|value| {
            ["code", "type"].into_iter().any(|field| {
                matches!(
                    value.get(field).and_then(Value::as_str),
                    Some(
                        "insufficient_quota"
                            | "usage_limit_exceeded"
                            | "usage_limit_reached"
                            | "billing_hard_limit_reached"
                            | "subscription_exhausted"
                    )
                )
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn decodes_only_structured_provider_bodies() {
        for (code, expected) in [
            ("insufficient_quota", true),
            ("usage_limit_exceeded", true),
            ("rate_limit_exceeded", false),
        ] {
            let body = json!({"error":{"type":code,"code":code,"message":"test"}}).to_string();
            let original =
                json!({"type":"provider.rate-limit","status":429,"response":{"body":body}});
            let data = data(&original);
            assert_eq!(data["error"]["code"], code);
            assert_eq!(data["response"], original["response"]);
            assert_eq!(is_exhausted(&original), expected);
            let error: nori_protocol::acp::v1::Error = serde_json::from_value(
                json!({"code":-32603,"message":"provider error","data":data}),
            )
            .unwrap();
            assert_eq!(
                nori_acp_host::is_subscription_exhausted(&anyhow::Error::new(error)),
                expected
            );
        }
        let prose =
            json!({"type":"provider.rate-limit","status":429,"message":"insufficient_quota"});
        assert!(!is_exhausted(&prose));
        let wrapped = json!({"type":"provider.quota","bodyString":"{\"error\":{\"code\":\"insufficient_quota\"}}"});
        assert!(is_exhausted(&wrapped));
    }
}
