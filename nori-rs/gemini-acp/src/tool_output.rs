//! Project Core tool content while retaining the full result in ACP rawOutput.
use serde_json::Value;
use serde_json::json;

pub(crate) fn content(result: &Value) -> Vec<Value> {
    let mut content = Vec::new();
    if let Some(blocks) = result["data"]["content"].as_array() {
        for block in blocks {
            if matches!(
                block["type"].as_str(),
                Some("text" | "image" | "audio" | "resource" | "resource_link")
            ) {
                content.push(json!({"type":"content", "content":block}));
            }
        }
    }
    if let Some(parts) = result["responseParts"].as_array() {
        for part in parts {
            let block = if let Some(text) = part["text"].as_str() {
                Some(json!({"type":"content", "content":{"type":"text", "text":text}}))
            } else if let (Some(mime), Some(data)) = (
                part["inlineData"]["mimeType"].as_str(),
                part["inlineData"]["data"].as_str(),
            ) {
                if mime.starts_with("image/") {
                    Some(
                        json!({"type":"content", "content":{"type":"image", "mimeType":mime, "data":data}}),
                    )
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(block) = block
                && !content.contains(&block)
            {
                content.push(block);
            }
        }
    }
    if content.is_empty()
        && let Some(display) = result["resultDisplay"].as_str()
    {
        content.push(json!({"type":"content", "content":{"type":"text", "text":display}}));
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn keeps_representable_blocks_and_avoids_duplicate_model_parts() {
        let result = json!({"resultDisplay":"display", "data":{"content":[{"type":"text","text":"hello"},{"type":"image","mimeType":"image/png","data":"aGVsbG8="}],"structuredContent":{"value":7}}, "responseParts":[{"text":"hello"},{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}},{"text":"extra"}]});
        assert_eq!(
            content(&result),
            vec![
                json!({"type":"content","content":{"type":"text","text":"hello"}}),
                json!({"type":"content","content":{"type":"image","mimeType":"image/png","data":"aGVsbG8="}}),
                json!({"type":"content","content":{"type":"text","text":"extra"}}),
            ]
        );
        assert_eq!(result["data"]["structuredContent"], json!({"value":7}));
    }

    #[test]
    fn falls_back_to_display_when_no_content_is_representable() {
        assert_eq!(
            content(
                &json!({"resultDisplay":"done", "responseParts":[{"functionResponse":{"name":"tool","response":{"ok":true}}}]})
            ),
            vec![json!({"type":"content","content":{"type":"text","text":"done"}})]
        );
    }
}
