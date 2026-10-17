//! Native delivery from the backend to macOS Notification Center through the bundled CortexCode
//! notification app (`CortexCodeNotify.app`, UNUserNotificationCenter): proper "CortexCode"
//! notifications with its icon, replaceable in place, whose clicks come back to the plugin. macOS
//! needs the user's permission for it: the plugin asks when it starts (the system shows its
//! standard prompt). Without permission nothing is posted to macOS; the notification stays in
//! CortexCode's own list. There is no other native path (owner).

pub mod helper;

use cc_protocol::BoxFuture;
use cc_protocol::contracts::notify::NativeStatus;
use helper::{Helper, NativeActivation};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

/// One notification to deliver natively.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeRequest {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    /// Groups notifications in Notification Center (the project).
    pub thread: String,
    pub actions: Vec<String>,
    /// The method that delivered the previous version of this notification, when it is an update
    /// of one already on screen.
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NativeOutcome {
    pub method: String,
    pub detail: String,
}

/// A native delivery backend (the real one is [`MacNative`]; tests substitute their own).
pub trait NativeDelivery: Send + Sync + 'static {
    /// The notification app only: `Ok(Some)` when it showed the notification, `Ok(None)` when
    /// macOS does not allow it (yet), `Err` when it is allowed but failed.
    fn deliver_app(&self, request: NativeRequest) -> BoxFuture<'_, Result<Option<NativeOutcome>, String>>;
    /// Why the notification app cannot show notifications now (recorded with the notification).
    fn unavailable_reason(&self) -> BoxFuture<'_, String>;
    /// Ask macOS for permission when it has not been decided yet (at startup; the user answers).
    fn ask_if_undecided(&self) -> BoxFuture<'_, ()>;
    fn status(&self) -> BoxFuture<'_, NativeStatus>;
    /// Validate that a path is usable without showing anything. Ok carries a description.
    fn check(&self) -> BoxFuture<'_, Result<String, String>>;
    fn stop(&self) -> BoxFuture<'_, ()>;
}

pub struct MacNative {
    helper: Option<Arc<Helper>>,
    /// Why the helper is absent (empty when present).
    helper_note: String,
    request_authorization: bool,
    asked: AtomicBool,
    spool: PathBuf,
}

const USABLE: [&str; 3] = ["authorized", "provisional", "ephemeral"];

/// Where the notification app is: configured, beside the executable, or where build.rs put it.
pub fn locate_helper(configured: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = configured {
        return if Helper::present(&path) { Ok(path) } else { Err(format!("configured helper_app {} is not a complete app bundle", path.display())) };
    }
    let mut tried = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside = dir.join(helper::APP);
        if Helper::present(&beside) {
            return Ok(beside);
        }
        tried.push(beside.display().to_string());
    }
    match env!("CC_NOTIFY_HELPER_BUILD").split_once(':') {
        Some(("built", path)) => {
            let built = PathBuf::from(path);
            if Helper::present(&built) {
                return Ok(built);
            }
            tried.push(built.display().to_string());
            Err(format!("the notification app is missing (looked in {})", tried.join(", ")))
        }
        Some((_, reason)) => Err(format!("the notification app was not built: {reason}")),
        None => Err("the notification app was not built".into()),
    }
}

impl MacNative {
    /// `helper`: the located app or why there is none. Activations of the app's notifications go to
    /// `activations`.
    pub fn new(helper: Result<PathBuf, String>, request_authorization: bool, spool: PathBuf, activations: mpsc::UnboundedSender<NativeActivation>) -> Self {
        let (helper, helper_note) = match helper {
            Ok(app) => (Some(Arc::new(Helper::new(app, activations))), String::new()),
            Err(note) => (None, note),
        };
        Self { helper, helper_note, request_authorization, asked: AtomicBool::new(false), spool }
    }

    async fn authorization(&self) -> (String, String) {
        match &self.helper {
            None => ("absent".into(), self.helper_note.clone()),
            Some(helper) => match helper.authorization().await {
                Ok(state) => (state, String::new()),
                Err(error) => ("error".into(), error),
            },
        }
    }

    /// Ask for permission once per process; the answer comes from the user, so nothing waits on it.
    fn ask_once(&self) -> bool {
        let Some(helper) = &self.helper else { return false };
        if !self.request_authorization || self.asked.swap(true, Ordering::SeqCst) {
            return false;
        }
        let helper = helper.clone();
        tokio::spawn(async move {
            match helper.authorize().await {
                Ok(state) => tracing::info!(%state, "notification permission answered"),
                Err(error) => tracing::warn!(%error, "notification permission request failed"),
            }
        });
        true
    }

    fn describe(&self, authorization: &str, error: &str) -> String {
        match authorization {
            "absent" => format!("not shown by macOS: the CortexCode notification app is unavailable ({error})"),
            "error" => format!("not shown by macOS: the CortexCode notification app failed ({error})"),
            "denied" => "not shown by macOS: notifications for CortexCode are turned off (System Settings > Notifications > CortexCode)".into(),
            "not_determined" => "not shown by macOS: CortexCode asked for permission to send notifications and is waiting for your answer".into(),
            other if USABLE.contains(&other) => "the CortexCode notification app".into(),
            other => format!("not shown by macOS: the notification permission is {other}"),
        }
    }
}

impl NativeDelivery for MacNative {
    fn deliver_app(&self, request: NativeRequest) -> BoxFuture<'_, Result<Option<NativeOutcome>, String>> {
        Box::pin(async move {
            let (authorization, _) = self.authorization().await;
            if !USABLE.contains(&authorization.as_str()) {
                return Ok(None);
            }
            let Some(helper) = &self.helper else { return Ok(None) };
            match helper.post(&request, &self.spool).await {
                Ok(()) => {
                    let detail = if request.replaces.is_some() { "replaced in Notification Center by the CortexCode notification app" } else { "delivered by the CortexCode notification app" };
                    Ok(Some(NativeOutcome { method: "helper".into(), detail: detail.into() }))
                }
                Err(error) => Err(format!("the CortexCode notification app could not post it ({error})")),
            }
        })
    }

    fn unavailable_reason(&self) -> BoxFuture<'_, String> {
        Box::pin(async move {
            let (authorization, error) = self.authorization().await;
            self.describe(&authorization, &error)
        })
    }

    fn ask_if_undecided(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let (authorization, _) = self.authorization().await;
            if authorization == "not_determined" {
                self.ask_once();
            }
        })
    }

    fn status(&self) -> BoxFuture<'_, NativeStatus> {
        Box::pin(async move {
            let (authorization, error) = self.authorization().await;
            let helper_ok = USABLE.contains(&authorization.as_str());
            let method = if helper_ok { "helper" } else { "none" };
            NativeStatus {
                method: method.into(),
                activation_capable: helper_ok,
                helper_path: self.helper.as_ref().map(|h| h.app.display().to_string()).unwrap_or_default(),
                authorization,
                osascript: false,
                detail: if error.is_empty() { String::new() } else { error },
            }
        })
    }

    fn check(&self) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let (authorization, error) = self.authorization().await;
            match authorization.as_str() {
                // The notification app ran and failed: a fault.
                "error" => Err(self.describe(&authorization, &error)),
                // Permission is the user's answer, and without the app the list still works.
                _ => Ok(self.describe(&authorization, &error)),
            }
        })
    }

    fn stop(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(helper) = &self.helper {
                helper.stop().await;
            }
        })
    }
}
