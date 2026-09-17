#![deny(unsafe_code)]
//! Narrow provider boundary: only sanitized local structure enters the model.
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, process::Stdio};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairContext {
    pub selector: String,
    pub html: String,
    pub bounds: [f64; 4],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub selector: String,
}

#[derive(Debug, thiserror::Error)]
#[error("Selector repair provider unavailable or returned invalid output")]
pub struct ProviderError;

pub trait SelectorProvider: Send + Sync {
    fn repair<'a>(
        &'a self,
        context: &'a RepairContext,
    ) -> Pin<Box<dyn Future<Output = Result<Candidate, ProviderError>> + Send + 'a>>;
}

/// An explicitly configured local model adapter. No shell or implicit cloud egress.
pub struct LocalProvider;
impl SelectorProvider for LocalProvider {
    fn repair<'a>(
        &'a self,
        context: &'a RepairContext,
    ) -> Pin<Box<dyn Future<Output = Result<Candidate, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let executable = std::env::var_os("CLINCH_REPAIR_PROVIDER").ok_or(ProviderError)?;
            let mut command = tokio::process::Command::new(executable);
            // Optional single script path, passed directly without shell parsing.
            if let Some(script) = std::env::var_os("CLINCH_REPAIR_PROVIDER_SCRIPT") {
                command.arg(script);
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| ProviderError)?;
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                let bytes = serde_json::to_vec(context).map_err(|_| ProviderError)?;
                let mut stdin = child.stdin.take().ok_or(ProviderError)?;
                stdin.write_all(&bytes).await.map_err(|_| ProviderError)?;
                drop(stdin);
                let mut bytes = Vec::new();
                child
                    .stdout
                    .take()
                    .ok_or(ProviderError)?
                    .take(4097)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|_| ProviderError)?;
                if bytes.len() > 4096 {
                    return Err(ProviderError);
                }
                if !child.wait().await.map_err(|_| ProviderError)?.success() {
                    return Err(ProviderError);
                }
                let candidate: Candidate =
                    serde_json::from_slice(&bytes).map_err(|_| ProviderError)?;
                if candidate.selector.trim().is_empty() || candidate.selector.len() > 2048 {
                    return Err(ProviderError);
                }
                Ok(candidate)
            })
            .await
            .map_err(|_| ProviderError)?
        })
    }
}
