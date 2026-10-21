//! A plugin's own record kept in the app's main store, as one global setting (`memory.settings`,
//! path `<plugin>.<name>`): read once when the plugin starts serving, held in memory while it runs,
//! and every change written through in order (when a write fails, the latest value is written again
//! until the store takes it). A record the plugin kept in a JSON file before is read from that
//! file once, written to the store and the file removed.

use crate::contracts::memory::methods;
use crate::plugin::Ctx;
use crate::{RpcError, codes};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// Between two attempts to write a value the store did not take.
const RETRY: Duration = Duration::from_secs(1);

pub struct Stored<T> {
    path: &'static str,
    value: Mutex<T>,
    writes: Mutex<Option<mpsc::UnboundedSender<Value>>>,
}

impl<T: Serialize + DeserializeOwned + Default + Clone + Send + 'static> Stored<T> {
    /// The record at settings path `path` (two or more dotted segments: `source.settings`).
    pub fn new(path: &'static str) -> Self {
        Self { path, value: Mutex::new(T::default()), writes: Mutex::new(None) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read the record from the store (or once from `legacy`, the JSON file it was kept in) and
    /// start writing changes through. Called when the plugin starts serving; a second call does
    /// nothing.
    pub async fn load(&self, ctx: &Ctx, legacy: Option<PathBuf>) -> Result<(), RpcError> {
        if self.writes.lock().map(|writes| writes.is_some()).unwrap_or(true) {
            return Ok(());
        }
        // The store's plugin may start after this one: its answer is waited for.
        let stored = loop {
            match ctx.call(methods::SETTINGS_GET, json!({ "path": self.path })).await {
                Ok(setting) => break setting.get("value").cloned().and_then(|value| serde_json::from_value::<T>(value).ok()),
                Err(error) if error.code == codes::NOT_FOUND => break None,
                Err(error) if matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING) => {
                    tokio::time::sleep(RETRY).await;
                }
                Err(error) => return Err(error),
            }
        };
        let imported = match (&stored, legacy.as_deref()) {
            (None, Some(file)) => read_legacy::<T>(file),
            _ => None,
        };
        *self.lock() = stored.or_else(|| imported.clone()).unwrap_or_default();
        let (tx, rx) = mpsc::unbounded_channel();
        spawn_writer(ctx.clone(), self.path, rx);
        if let Ok(mut writes) = self.writes.lock() {
            *writes = Some(tx);
        }
        if imported.is_some() {
            self.write(&self.get());
            if let Some(file) = legacy
                && let Err(error) = std::fs::remove_file(&file)
            {
                tracing::warn!(file = %file.display(), %error, "cannot remove the imported record file");
            }
        }
        Ok(())
    }

    /// The record as it is now.
    pub fn get(&self) -> T {
        self.lock().clone()
    }

    /// Change the record and write it through; returns what `change` returns.
    pub fn update<R>(&self, change: impl FnOnce(&mut T) -> R) -> R {
        let (result, value) = {
            let mut value = self.lock();
            let result = change(&mut value);
            (result, value.clone())
        };
        self.write(&value);
        result
    }

    fn write(&self, value: &T) {
        let Ok(json) = serde_json::to_value(value) else { return };
        match self.writes.lock().ok().and_then(|writes| writes.clone()) {
            Some(writes) => {
                let _ = writes.send(json);
            }
            None => tracing::warn!(path = self.path, "a record changed before it was loaded from the store: not written"),
        }
    }
}

fn read_legacy<T: DeserializeOwned>(file: &Path) -> Option<T> {
    let bytes = std::fs::read(file).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(file = %file.display(), %error, "an old record file is not valid JSON: not imported");
            None
        }
    }
}

/// Write the latest value through, in order; a value the store does not take is tried again.
fn spawn_writer(ctx: Ctx, path: &'static str, mut rx: mpsc::UnboundedReceiver<Value>) {
    tokio::spawn(async move {
        while let Some(mut value) = rx.recv().await {
            // Only the latest of several queued values matters.
            while let Ok(newer) = rx.try_recv() {
                value = newer;
            }
            loop {
                match ctx.call(methods::SETTINGS_SET, json!({ "path": path, "value": value, "by": "plugin" })).await {
                    Ok(_) => break,
                    Err(error) => {
                        tracing::warn!(path, %error.message, "the store did not take a record; trying again");
                        tokio::time::sleep(RETRY).await;
                        if let Ok(newer) = rx.try_recv() {
                            value = newer;
                        }
                    }
                }
            }
        }
    });
}
