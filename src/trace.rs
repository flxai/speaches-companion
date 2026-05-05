use std::path::Path;

use anyhow::Context;
use serde_json::Value;

#[derive(Debug, Default)]
pub struct TraceWriter {
    events: Vec<Value>,
}

impl TraceWriter {
    pub fn push(&mut self, event: Value) {
        self.events.push(event);
    }

    pub async fn write_jsonl(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create trace directory {}", parent.display())
            })?;
        }

        let mut output = String::new();
        for event in &self.events {
            output.push_str(&serde_json::to_string(event)?);
            output.push('\n');
        }

        tokio::fs::write(path, output)
            .await
            .with_context(|| format!("failed to write trace {}", path.display()))?;
        Ok(())
    }
}
