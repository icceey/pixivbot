use teloxide::types::ChatId;

/// Maximum number of subscriptions per page
pub(crate) const PAGE_SIZE: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPaginationAction {
    Noop,
    Page {
        page: usize,
        target_chat_id: Option<ChatId>,
        is_channel: bool,
    },
}

/// 批量操作结果收集器
pub(crate) struct BatchResult {
    success: Vec<String>,
    failed: Vec<String>,
}

impl BatchResult {
    pub(crate) fn new() -> Self {
        Self {
            success: Vec::new(),
            failed: Vec::new(),
        }
    }

    pub(crate) fn add_success(&mut self, item: String) {
        self.success.push(item);
    }

    pub(crate) fn add_failure(&mut self, item: String) {
        self.failed.push(item);
    }

    pub(crate) fn has_success(&self) -> bool {
        !self.success.is_empty()
    }

    /// 构建成功/失败列表的响应消息
    pub(crate) fn build_response(&self, success_prefix: &str, failure_prefix: &str) -> String {
        self.build_response_with_suffix(success_prefix, failure_prefix, None)
    }

    /// 构建成功/失败列表的响应消息，在成功列表后添加可选后缀
    pub(crate) fn build_response_with_suffix(
        &self,
        success_prefix: &str,
        failure_prefix: &str,
        success_suffix: Option<&str>,
    ) -> String {
        let mut response = String::new();

        if !self.success.is_empty() {
            response.push_str(success_prefix);
            response.push('\n');
            for item in &self.success {
                response.push_str(&format!("  • {}\n", item));
            }
            if let Some(suffix) = success_suffix {
                response.push_str(suffix);
            }
        }

        if !self.failed.is_empty() {
            if !response.is_empty() {
                response.push('\n');
            }
            response.push_str(failure_prefix);
            response.push('\n');
            for item in &self.failed {
                response.push_str(&format!("  • {}\n", item));
            }
        }

        response
    }
}
