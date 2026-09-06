use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

const BYTES_PER_ESTIMATED_TOKEN: u64 = 4;
const RECENT_CALL_LIMIT: usize = 10;
const RECENT_LABEL_LIMIT: usize = 80;

/// Runtime-local usage counters. Only aggregate sizes and counts are retained.
#[derive(Debug, Default)]
pub(crate) struct ServiceUsage {
    request_count: AtomicU64,
    tool_call_count: AtomicU64,
    error_count: AtomicU64,
    input_bytes: AtomicU64,
    output_bytes: AtomicU64,
    last_success_at_ms: AtomicU64,
    last_failure_at_ms: AtomicU64,
    recent_calls: Mutex<VecDeque<RecentCall>>,
}

impl ServiceUsage {
    pub(crate) fn record(
        &self,
        input_bytes: usize,
        output_bytes: usize,
        is_tool_call: bool,
        is_error: bool,
    ) {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        if is_tool_call {
            self.tool_call_count.fetch_add(1, Ordering::Relaxed);
        }
        if is_error {
            self.error_count.fetch_add(1, Ordering::Relaxed);
        }
        self.input_bytes
            .fetch_add(input_bytes as u64, Ordering::Relaxed);
        self.output_bytes
            .fetch_add(output_bytes as u64, Ordering::Relaxed);
        self.mark_outcome(is_error);
    }

    /// Record an auth or transport rejection without counting it as a billed request.
    pub(crate) fn mark_failure(&self) {
        self.mark_outcome(true);
        self.remember("auth", "", true);
    }

    pub(crate) fn remember(&self, method: &str, tool: &str, is_error: bool) {
        if method.starts_with("notifications/") {
            return;
        }
        let at_ms = unix_now_ms();
        if at_ms == 0 {
            return;
        }
        let Ok(mut recent) = self.recent_calls.lock() else {
            return;
        };
        recent.push_back(RecentCall {
            at_ms,
            method: truncate_label(method),
            tool: truncate_label(tool),
            is_error,
        });
        while recent.len() > RECENT_CALL_LIMIT {
            recent.pop_front();
        }
    }

    fn mark_outcome(&self, is_error: bool) {
        let now = unix_now_ms();
        if now == 0 {
            return;
        }
        if is_error {
            self.last_failure_at_ms.store(now, Ordering::Relaxed);
        } else {
            self.last_success_at_ms.store(now, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self, workspace_id: &str, service: &str) -> ServiceUsageStats {
        let input_bytes = self.input_bytes.load(Ordering::Relaxed);
        let output_bytes = self.output_bytes.load(Ordering::Relaxed);
        let estimated_input_tokens = estimate_tokens(input_bytes);
        let estimated_output_tokens = estimate_tokens(output_bytes);

        ServiceUsageStats {
            workspace_id: workspace_id.to_string(),
            service: service.to_string(),
            request_count: self.request_count.load(Ordering::Relaxed),
            tool_call_count: self.tool_call_count.load(Ordering::Relaxed),
            error_count: self.error_count.load(Ordering::Relaxed),
            input_bytes,
            output_bytes,
            estimated_input_tokens,
            estimated_output_tokens,
            estimated_tokens: estimated_input_tokens.saturating_add(estimated_output_tokens),
            last_success_at_ms: nonzero_ms(self.last_success_at_ms.load(Ordering::Relaxed)),
            last_failure_at_ms: nonzero_ms(self.last_failure_at_ms.load(Ordering::Relaxed)),
            recent_calls: self
                .recent_calls
                .lock()
                .map(|recent| recent.iter().rev().cloned().collect())
                .unwrap_or_default(),
        }
    }

    pub(crate) fn empty(workspace_id: &str, service: &str) -> ServiceUsageStats {
        ServiceUsageStats {
            workspace_id: workspace_id.to_string(),
            service: service.to_string(),
            request_count: 0,
            tool_call_count: 0,
            error_count: 0,
            input_bytes: 0,
            output_bytes: 0,
            estimated_input_tokens: 0,
            estimated_output_tokens: 0,
            estimated_tokens: 0,
            last_success_at_ms: None,
            last_failure_at_ms: None,
            recent_calls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceUsageStats {
    pub workspace_id: String,
    pub service: String,
    pub request_count: u64,
    pub tool_call_count: u64,
    pub error_count: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub estimated_input_tokens: u64,
    pub estimated_output_tokens: u64,
    pub estimated_tokens: u64,
    pub last_success_at_ms: Option<u64>,
    pub last_failure_at_ms: Option<u64>,
    pub recent_calls: Vec<RecentCall>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecentCall {
    pub at_ms: u64,
    pub method: String,
    pub tool: String,
    pub is_error: bool,
}

fn estimate_tokens(bytes: u64) -> u64 {
    bytes
        .saturating_add(BYTES_PER_ESTIMATED_TOKEN - 1)
        / BYTES_PER_ESTIMATED_TOKEN
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn nonzero_ms(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

fn truncate_label(value: &str) -> String {
    let mut label = value.trim().to_string();
    if label.chars().count() > RECENT_LABEL_LIMIT {
        label = label.chars().take(RECENT_LABEL_LIMIT).collect();
    }
    label
}

#[cfg(test)]
mod tests {
    use super::{estimate_tokens, ServiceUsage};

    #[test]
    fn records_counts_and_json_sizes_without_retaining_content() {
        let usage = ServiceUsage::default();
        usage.record(8, 9, true, false);
        usage.record(4, 7, false, true);

        let stats = usage.snapshot("workspace-1", "mcp");
        assert_eq!(stats.request_count, 2);
        assert_eq!(stats.tool_call_count, 1);
        assert_eq!(stats.error_count, 1);
        assert_eq!(stats.input_bytes, 12);
        assert_eq!(stats.output_bytes, 16);
        assert_eq!(stats.estimated_input_tokens, 3);
        assert_eq!(stats.estimated_output_tokens, 4);
        assert_eq!(stats.estimated_tokens, 7);
        assert!(stats.last_success_at_ms.is_some());
        assert!(stats.last_failure_at_ms.is_some());
        assert!(stats.last_failure_at_ms >= stats.last_success_at_ms);
    }

    #[test]
    fn auth_failure_updates_last_failure_without_counting_a_request() {
        let usage = ServiceUsage::default();
        usage.mark_failure();
        let stats = usage.snapshot("workspace-1", "mcp");
        assert_eq!(stats.request_count, 0);
        assert_eq!(stats.error_count, 0);
        assert!(stats.last_success_at_ms.is_none());
        assert!(stats.last_failure_at_ms.is_some());
        assert_eq!(stats.recent_calls.len(), 1);
        assert_eq!(stats.recent_calls[0].method, "auth");
        assert!(stats.recent_calls[0].is_error);
    }

    #[test]
    fn remembers_latest_ten_calls_newest_first() {
        let usage = ServiceUsage::default();
        for index in 0..12 {
            usage.remember("tools/call", &format!("tool-{index}"), index % 2 == 0);
        }
        let stats = usage.snapshot("workspace-1", "mcp");
        assert_eq!(stats.recent_calls.len(), 10);
        assert_eq!(stats.recent_calls[0].tool, "tool-11");
        assert_eq!(stats.recent_calls[9].tool, "tool-2");
        assert!(stats.recent_calls.iter().all(|call| call.method == "tools/call"));
    }

    #[test]
    fn skips_notification_methods() {
        let usage = ServiceUsage::default();
        usage.remember("notifications/initialized", "", false);
        usage.remember("initialize", "", false);
        let stats = usage.snapshot("workspace-1", "mcp");
        assert_eq!(stats.recent_calls.len(), 1);
        assert_eq!(stats.recent_calls[0].method, "initialize");
    }

    #[test]
    fn token_estimation_rounds_up_each_payload() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
    }
}
