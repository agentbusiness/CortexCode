//! Current bytes → owning language pack → bounded value origins. No persistent trace cache.
use crate::{host::Host, langs::Snapshot};
use cc_protocol::{
    RpcError, codes,
    contracts::{lang, memory},
};
use std::path::Path;
use tokio::io::AsyncReadExt;

async fn bytes(path: &Path) -> Result<Vec<u8>, RpcError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    if !file
        .metadata()
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?
        .is_file()
    {
        return Err(RpcError::invalid_params("Choose a Python source file."));
    }
    let mut data = Vec::new();
    file.take(512 * 1024 + 1)
        .read_to_end(&mut data)
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
    if data.len() > 512 * 1024 {
        return Err(RpcError::invalid_params(
            "Value tracing accepts files under 512 KiB; read the selected function instead.",
        ));
    }
    Ok(data)
}

pub async fn trace(
    root: &Path,
    host: &dyn Host,
    snapshot: &Snapshot,
    p: memory::ValueTraceParams,
) -> Result<lang::ValueTrace, RpcError> {
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let path = tokio::fs::canonicalize(root.join(&p.path))
        .await
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| RpcError::invalid_params("Choose a file inside this project's workspace."))?
        .to_string_lossy()
        .replace('\\', "/");
    let data = bytes(&path).await?;
    let text = String::from_utf8(data.clone())
        .map_err(|_| RpcError::invalid_params("Value tracing needs UTF-8 source."))?;
    let pick = snapshot.pick(&relative, Some(&text)).ok_or_else(|| {
        RpcError::new(
            codes::UNAVAILABLE,
            "No language plugin covers this file; read its source.",
        )
    })?;
    let request = lang::ValueTraceParams {
        file: lang::SourceFile {
            path: relative.clone(),
            text,
        },
        line: p.line,
        name: p.name.clone(),
        max_steps: p.max_steps.clamp(1, 80),
    };
    let value = host
        .call(
            &format!("{}.{}", pick.plugin.category, lang::methods::VALUE_TRACE),
            serde_json::to_value(request).map_err(|e| RpcError::internal(e.to_string()))?,
        )
        .await?;
    let mut result: lang::ValueTrace = serde_json::from_value(value)
        .map_err(|e| RpcError::internal(format!("Unreadable value trace: {e}")))?;
    if result.path != relative
        || result.name != p.name
        || result.line != p.line
        || result.steps.len() > p.max_steps.clamp(1, 80) as usize
    {
        return Err(RpcError::internal(
            "The language plugin returned a trace for a different request.",
        ));
    }
    if bytes(&path).await? != data {
        return Err(RpcError::invalid_params(
            "The file changed while tracing it; read the current source and ask again.",
        ));
    }
    result.content_hash = crate::facts::hash_hex(&data);
    Ok(result)
}
