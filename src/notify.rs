//! Best-effort desktop notifications for incoming messages.
//!
//! Fired when a message lands in a chat the user isn't currently looking at
//! (terminal unfocused, or a different peer selected). Delivery is
//! fire-and-forget on a detached thread so a slow or missing notification
//! daemon never stalls the TUI; failures are logged at debug level. Stdout and
//! stderr stay untouched — they belong to the terminal UI.

/// Show a desktop notification for a message `body` received from `from`.
pub fn message(from: &str, body: &str) {
    let summary = format!("New message from {from}");
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new()
            .appname("cweldrop")
            .summary(&summary)
            .body(&body)
            .show()
        {
            tracing::debug!(error = %e, "desktop notification failed");
        }
    });
}
