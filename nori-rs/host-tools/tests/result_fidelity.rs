use nori_host_tools::HostToolOutput;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn preserves_opaque_mcp_content_structured_result_and_metadata() {
    let result = json!({
        "content": [
            {"type": "text", "text": "done", "annotations": {"audience": ["assistant"]}},
            {"type": "image", "data": "YWJj", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///data", "text": "body"}},
            {"type": "future-block", "value": {"nested": true}}
        ],
        "structuredContent": {"count": 2, "nested": [1, 2]},
        "_meta": {"source": "fixture"},
        "isError": false
    });
    let output = HostToolOutput::from_mcp_result(result.clone());
    assert_eq!(output.text, "done");
    assert!(!output.is_error);
    assert_eq!(output.as_mcp_result(), result);
}

#[test]
fn legacy_text_and_correlated_cancellation_have_mcp_results() {
    assert_eq!(
        HostToolOutput::text("ok").as_mcp_result(),
        json!({
            "content": [{"type": "text", "text": "ok"}], "isError": false,
        })
    );
    let cancelled = HostToolOutput::cancelled("call-42", "delegate_task");
    assert!(cancelled.is_error);
    assert_eq!(
        cancelled.as_mcp_result()["structuredContent"],
        json!({
            "status": "cancelled", "call_id": "call-42", "tool": "delegate_task",
        })
    );
}
