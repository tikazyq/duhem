//! Page network recorder operations and the request-window wire types.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{NetworkBatch, Page, PwError};

impl Page {
    /// Drain recorded network responses from `cursor` onward. Returns
    /// the batch plus the next cursor; `api/observe` polls this within
    /// its `timeout:` window. See [`super::NetworkEvent`].
    pub async fn poll_network(&self, cursor: u64) -> Result<NetworkBatch, PwError> {
        let mut req = self.p();
        req["cursor"] = json!(cursor);
        let v = self.conn.request("pollNetwork", req).await?;
        serde_json::from_value(v).map_err(|e| PwError(format!("pollNetwork decode: {e}")))
    }

    /// Begin recording request lifecycle events before the first step runs.
    pub async fn enable_request_recording(&self) -> Result<(), PwError> {
        self.conn
            .request("enableRequestRecording", self.p())
            .await
            .map(|_| ())
    }

    pub(crate) async fn request_window_batch(
        &self,
        from_ms: f64,
        until_ms: f64,
        cursor: Option<usize>,
    ) -> Result<RequestWindowBatch, PwError> {
        let mut req = self.p();
        req["fromMs"] = json!(from_ms);
        req["untilMs"] = json!(until_ms);
        req["cursor"] = json!(cursor);
        let response = self.conn.request("requestWindowBatch", req).await?;
        serde_json::from_value(response)
            .map_err(|e| PwError(format!("requestWindowBatch decode: {e}")))
    }
}

#[derive(Deserialize)]
pub(crate) struct RequestWindowBatch {
    pub requests: Vec<WindowRequest>,
    pub cursor: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct WindowRequest {
    pub method: String,
    pub url: String,
    pub started_ms: f64,
    pub status: Option<u16>,
    pub state: String,
    pub failure: Option<String>,
}
