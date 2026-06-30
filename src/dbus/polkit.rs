//! polkit (PolicyKit1) client. powerd authorizes callers of its privileged
//! methods against an active local session: the shipped action defaults to
//! `allow_active=yes` / `allow_inactive=no` / `allow_any=no`, so power control
//! belongs to whoever is physically at the machine, not to every member of a
//! group. Fail-closed — if polkitd is unreachable or errors, deny.

use std::collections::HashMap;

use tracing::warn;
use zbus::zvariant::Value;

/// The single polkit action gating powerd's privileged methods.
pub const ACTION_ID: &str = "org.hyprstate.power1.manage";

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Authority {
    /// `CheckAuthorization(in (sa{sv}) subject, in s action_id,
    /// in a{ss} details, in u flags, in s cancellation_id,
    /// out (bba{ss}) result)`.
    fn check_authorization(
        &self,
        subject: &(String, HashMap<String, Value<'_>>),
        action_id: &str,
        details: HashMap<String, String>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

/// Is `sender` (a unique bus name like `:1.42`) authorized for [`ACTION_ID`]?
/// Fail-closed on an empty sender or any polkit error.
pub async fn caller_authorized(conn: &zbus::Connection, sender: &str) -> bool {
    if sender.is_empty() {
        warn!("polkit: empty caller sender — denying");
        return false;
    }
    let authority = match AuthorityProxy::new(conn).await {
        Ok(a) => a,
        Err(e) => {
            warn!("polkit: authority unavailable ({e}) — denying");
            return false;
        }
    };
    let mut subject_details: HashMap<String, Value<'_>> = HashMap::new();
    subject_details.insert("name".to_string(), Value::from(sender));
    let subject = ("system-bus-name".to_string(), subject_details);
    // flags=0: no interactive auth. The active local session is allowed
    // outright by the policy; inactive/remote callers are simply denied.
    match authority
        .check_authorization(&subject, ACTION_ID, HashMap::new(), 0, "")
        .await
    {
        Ok((authorized, _challenge, _details)) => authorized,
        Err(e) => {
            warn!("polkit: CheckAuthorization failed ({e}) — denying");
            false
        }
    }
}
