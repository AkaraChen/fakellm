//! The in-memory state behind `previous_response_id`, `GET /v1/responses/{id}` and
//! `DELETE /v1/responses/{id}`: response id to the response object and the conversation that
//! produced it. Cleared by `reset`. Bounded to [`CAPACITY`] responses, oldest evicted first: every entry holds
//! its whole conversation, so a long `previous_response_id` chain grows quadratically.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::ir::Message;

/// Maximum number of stored responses.
pub const CAPACITY: usize = 1000;

/// One stored response.
#[derive(Clone, Debug)]
pub struct StoredResponse {
    pub id: String,
    /// Unix seconds from the server clock when the response was created.
    pub created_at: i64,
    /// The full response object exactly as the sync API returns it (`GET` serves this).
    pub response: Value,
    /// The conversation after this response: the request's messages (including everything
    /// resolved from earlier `previous_response_id` links, excluding the system message made
    /// from `instructions`) followed by the output items converted to IR messages. A request
    /// naming this id starts from a copy of it.
    pub conversation: Vec<Message>,
}

#[derive(Default)]
struct Inner {
    by_id: HashMap<String, Arc<StoredResponse>>,
    /// Ids oldest first, for stable listing.
    order: VecDeque<String>,
}

#[derive(Default)]
pub struct ResponseStore {
    inner: Mutex<Inner>,
}

impl ResponseStore {
    /// Stores `response`, replacing an entry with the same id.
    pub fn insert(&self, response: StoredResponse) {
        let mut inner = self.inner.lock();
        let id = response.id.clone();
        if inner.by_id.insert(id.clone(), Arc::new(response)).is_none() {
            inner.order.push_back(id);
            while inner.order.len() > CAPACITY {
                if let Some(old) = inner.order.pop_front() {
                    inner.by_id.remove(&old);
                }
            }
        }
    }

    /// [`insert`](Self::insert) for an entry that may be absent (`store: false`).
    pub fn insert_opt(&self, response: Option<StoredResponse>) {
        if let Some(r) = response {
            self.insert(r);
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<StoredResponse>> {
        self.inner.lock().by_id.get(id).cloned()
    }

    /// Returns whether the id existed.
    pub fn remove(&self, id: &str) -> bool {
        let mut inner = self.inner.lock();
        let existed = inner.by_id.remove(id).is_some();
        if existed {
            inner.order.retain(|i| i != id);
        }
        existed
    }

    /// `(id, created_at)` oldest first.
    pub fn list(&self) -> Vec<(String, i64)> {
        let inner = self.inner.lock();
        inner
            .order
            .iter()
            .filter_map(|id| inner.by_id.get(id).map(|r| (id.clone(), r.created_at)))
            .collect()
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.by_id.clear();
        inner.order.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().by_id.len()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn evicts_oldest_beyond_capacity() {
        let s = ResponseStore::default();
        for i in 0..=CAPACITY {
            s.insert(stored(&format!("resp_{i}"), 0));
        }
        assert_eq!(s.len(), CAPACITY);
        assert!(s.get("resp_0").is_none() && s.get("resp_1").is_some());
    }

    fn stored(id: &str, at: i64) -> StoredResponse {
        StoredResponse {
            id: id.into(),
            created_at: at,
            response: json!({"id": id}),
            conversation: Vec::new(),
        }
    }

    #[test]
    fn insert_get_remove_list_clear() {
        let s = ResponseStore::default();
        s.insert(stored("resp_a", 1));
        s.insert(stored("resp_b", 2));
        s.insert(stored("resp_a", 3));
        assert_eq!(s.len(), 2);
        assert_eq!(s.get("resp_a").unwrap().created_at, 3);
        assert_eq!(s.list(), [("resp_a".to_owned(), 3), ("resp_b".to_owned(), 2)]);
        assert!(s.remove("resp_a"));
        assert!(!s.remove("resp_a"));
        assert!(s.get("resp_a").is_none());
        assert_eq!(s.list().len(), 1);
        s.clear();
        assert_eq!(s.len(), 0);
    }
}
