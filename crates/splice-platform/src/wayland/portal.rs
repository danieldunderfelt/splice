//! Minimal portal plumbing over raw zbus.
//!
//! The workspace pins ashpd with only its `tokio` feature, so the `input_capture`,
//! `remote_desktop` and `clipboard` wrapper modules are compiled out; the portal
//! interfaces are spoken directly here. Only the pieces Splice needs are implemented.
//!
//! Portal Response results and signal options are `a{sv}` dicts. splice-platform has no
//! direct zvariant dependency (the derive macro would not resolve), so dicts are
//! deserialized as `HashMap<String, OwnedValue>` and fields are extracted with [`get`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use zbus::zvariant::{self, OwnedObjectPath, OwnedValue, Value};

use crate::{PlatformError, Result};

pub const DESKTOP_NAME: &str = "org.freedesktop.portal.Desktop";
pub const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
pub const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

pub type Options = HashMap<&'static str, Value<'static>>;
/// An `a{sv}` portal results/options dict.
pub type Results = HashMap<String, OwnedValue>;

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_token() -> String {
    let n = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("splice{}_{n}", std::process::id())
}

/// `/org/freedesktop/portal/request` / `session` paths embed the caller's unique bus
/// name with the leading `:` stripped and `.` replaced by `_`.
pub fn sender_id(conn: &zbus::Connection) -> String {
    conn.unique_name()
        .map(|n| n.as_str().trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default()
}

pub async fn proxy(conn: &zbus::Connection, iface: &'static str) -> Result<zbus::Proxy<'static>> {
    zbus::Proxy::new(conn, DESKTOP_NAME, DESKTOP_PATH, iface)
        .await
        .map_err(err_ctx("portal proxy"))
}

/// Proxy for a session object (`org.freedesktop.portal.Session` at the session path).
pub async fn session_proxy(
    conn: &zbus::Connection,
    session_path: &str,
) -> Result<zbus::Proxy<'static>> {
    let path = zvariant::ObjectPath::try_from(session_path.to_owned())
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("bad session path: {e}")))?;
    zbus::Proxy::new(conn, DESKTOP_NAME, path, SESSION_IFACE)
        .await
        .map_err(err_ctx("portal session proxy"))
}

/// Interface `version` property; 0 when the interface is absent.
pub async fn version(proxy: &zbus::Proxy<'_>) -> u32 {
    proxy.get_property::<u32>("version").await.unwrap_or(0)
}

/// Portal Request/Response round trip.
///
/// The Response signal arrives on a request object whose path is predictable from our
/// unique name plus the handle token we choose, so the signal subscription is installed
/// BEFORE the method call and no response can be missed. `args` receives the handle
/// token to place in the call's `handle_token` option.
pub async fn request<A>(
    conn: &zbus::Connection,
    proxy: &zbus::Proxy<'_>,
    method: &str,
    args: impl FnOnce(&str) -> A,
) -> Result<Results>
where
    A: serde::Serialize + zvariant::DynamicType,
{
    let token = next_token();
    let req_path = format!("{DESKTOP_PATH}/request/{}/{token}", sender_id(conn));
    let req_proxy = zbus::Proxy::new(
        conn,
        DESKTOP_NAME,
        req_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(err_ctx("portal request proxy"))?;
    let mut responses = req_proxy
        .receive_signal("Response")
        .await
        .map_err(err_ctx("Response signal"))?;
    let reply = proxy
        .call_method(method, &args(&token))
        .await
        .map_err(err_ctx(method))?;
    drop(reply);
    let msg = responses
        .next()
        .await
        .ok_or_else(|| PlatformError::Unavailable(format!("portal Response stream for {method} closed")))?;
    let (code, results): (u32, Results) = msg.body().deserialize().map_err(err_ctx(method))?;
    match code {
        0 => Ok(results),
        1 => Err(PlatformError::Permission(format!(
            "portal request {method} dismissed by the user"
        ))),
        code => Err(PlatformError::Other(anyhow::anyhow!(
            "portal request {method} failed with response code {code}"
        ))),
    }
}

/// Typed extraction of a signal body of the shape `(o session_handle, a{sv} options)`.
pub fn session_signal(msg: &zbus::Message) -> Option<(String, Results)> {
    let (path, options): (OwnedObjectPath, Results) = msg.body().deserialize().ok()?;
    Some((path.to_string(), options))
}

/// Extracts one field from a results dict, converted to the wanted type. Absent or
/// mistyped fields come back as None.
pub fn get<T: TryFrom<OwnedValue>>(results: &Results, key: &str) -> Option<T> {
    T::try_from(results.get(key)?.clone()).ok()
}

/// ObjectPath for a session handle string, for use as a method argument (`o`).
pub fn object_path(path: &str) -> Result<zvariant::ObjectPath<'static>> {
    zvariant::ObjectPath::try_from(path.to_owned())
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("bad object path {path}: {e}")))
}

pub fn err_ctx(context: &str) -> impl FnOnce(zbus::Error) -> PlatformError + '_ {
    move |e| PlatformError::Other(anyhow::anyhow!("{context}: {e}"))
}
