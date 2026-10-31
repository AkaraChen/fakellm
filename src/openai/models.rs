//! `GET /v1/models` and `GET /v1/models/{id}`.

use serde_json::{Value, json};

fn model(id: &str) -> Value {
    let created = 1_700_000_000 + crate::fallback::hash64(id.as_bytes()) % 50_000_000;
    json!({"id": id, "object": "model", "created": created, "owned_by": "openai"})
}

/// `{"object": "list", "data": [{"id", "object": "model", "created", "owned_by"}...]}`.
/// `created` is a fixed per-model value derived from the name, so it is stable.
pub fn list(models: &[String]) -> Value {
    json!({"object": "list", "data": models.iter().map(|m| model(m)).collect::<Vec<_>>()})
}

/// One model object, or `None` when it is not in the list.
pub fn get(models: &[String], id: &str) -> Option<Value> {
    models.iter().any(|m| m == id).then(|| model(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_and_get() {
        let ms = vec!["gpt-4o".to_owned(), "o3".to_owned()];
        let l = list(&ms);
        assert_eq!(l["object"], "list");
        assert_eq!(l["data"].as_array().unwrap().len(), 2);
        assert_eq!(l["data"][0]["object"], "model");
        assert_eq!(l["data"][0]["owned_by"], "openai");
        assert_eq!(get(&ms, "o3").unwrap(), l["data"][1]);
        assert!(get(&ms, "nope").is_none());
        let created = l["data"][0]["created"].as_u64().unwrap();
        assert!((1_700_000_000..1_750_000_000).contains(&created));
    }
}
