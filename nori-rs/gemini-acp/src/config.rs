use acp::schema::SessionConfigOption;
use agent_client_protocol as acp;
use serde_json::Value;
use serde_json::json;

pub(crate) struct Catalog {
    pub(crate) current: String,
    pub(crate) models: Vec<String>,
}
impl Catalog {
    pub(crate) fn from_engine(ready: &Value) -> acp::Result<Self> {
        let current = ready["model"]
            .as_str()
            .ok_or_else(acp::Error::internal_error)?
            .to_owned();
        let mut models = ready["models"]
            .as_array()
            .ok_or_else(acp::Error::internal_error)?
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if !models.contains(&current) {
            models.push(current.clone());
        }
        models.sort();
        models.dedup();
        Ok(Self { current, models })
    }
    pub(crate) fn options(&self) -> acp::Result<Vec<SessionConfigOption>> {
        let options = self
            .models
            .iter()
            .map(|name| json!({"name":name,"value":name}))
            .collect::<Vec<_>>();
        serde_json::from_value(json!([{"id":"model","name":"Model","category":"model","type":"select","currentValue":self.current,"options":options}])).map_err(acp::Error::into_internal_error)
    }
}
