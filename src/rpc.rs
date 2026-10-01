//! The daemon's control protocol ([ADR 0005]): JSON-RPC 2.0, one message
//! per line, over a Unix socket in the data directory ([`socket_path`]).
//!
//! A method is its params type, implementing [`Method`] next to the typed
//! function it calls: the server registers a handler for it in [`Methods`],
//! and the client sends it with `Client::call`. Both sides are this crate,
//! so their types can't drift apart. A [`StreamMethod`] also sends items
//! before it answers, as `stream` notifications.
//!
//! [ADR 0005]: ../../docs/adr/0005-control-the-daemon-over-a-unix-socket.md

/// Declare methods: for each, its params struct (the fields, in
/// camelCase on the wire, nothing else accepted), its name and its output.
macro_rules! define_methods {
    ($(
        $(#[$meta:meta])*
        $name:literal => $method:ident { $($(#[$field_meta:meta])* $field:ident: $type:ty),* $(,)? } -> $output:ty;
    )*) => {$(
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $method { $($(#[$field_meta])* pub $field: $type),* }

        impl $crate::rpc::Method for $method {
            const NAME: &'static str = $name;
            type Output = $output;
        }
    )*};
}
pub(crate) use define_methods;

mod methods;
#[cfg(unix)]
mod server;

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::mpsc;

pub use methods::*;
#[cfg(unix)]
pub use server::{RpcServer, ServeError, claim as claim_socket};

/// The socket's file name in the data directory.
pub const SOCKET_FILE_NAME: &str = "ferry.sock";

/// The longest line either side accepts; a longer one closes the
/// connection.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Where the daemon with data directory `data_dir` listens.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOCKET_FILE_NAME)
}

/// A request the daemon answers: its params, which name the method.
pub trait Method: Serialize + DeserializeOwned + Send + 'static {
    /// The method's name on the wire, `<owner>.<action>`, e.g.
    /// `"share.text"`.
    const NAME: &'static str;
    type Output: Serialize + DeserializeOwned + Send + 'static;
}

/// A method that sends items before it answers, e.g. events.
pub trait StreamMethod: Method {
    type Item: Serialize + DeserializeOwned + Send + 'static;
}

/// JSON-RPC's error codes, and the one for the daemon's own errors.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// A method refused or failed: `data.code` says why.
    pub const FAILED: i64 = -32000;
}

/// An error answer: JSON-RPC's `code` and `message`, and the daemon's
/// own code (as the core and plugins name their errors, e.g.
/// `device_not_paired`) with any detail from the device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorData {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RpcError {
    pub fn new(code: i64, error_code: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(ErrorData {
                code: error_code.to_owned(),
                detail: None,
            }),
        }
    }

    /// A method's own failure, `error_code` being what clients match on.
    pub fn failed(error_code: &str, message: impl Into<String>) -> Self {
        Self::new(code::FAILED, error_code, message)
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, "invalid_params", message)
    }

    pub fn internal() -> Self {
        Self::new(code::INTERNAL_ERROR, "internal_error", "internal error")
    }

    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        if let Some(data) = &mut self.data {
            data.detail = detail;
        }
        self
    }

    /// The daemon's code for it, e.g. `device_not_paired`.
    pub fn error_code(&self) -> &str {
        self.data.as_ref().map_or("", |data| data.code.as_str())
    }

    pub fn detail(&self) -> Option<&str> {
        self.data.as_ref()?.detail.as_deref()
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        if let Some(detail) = self.detail() {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

/// An error a method returns: its code for clients, its message, and any
/// detail from the device. Every error type a handler returns implements
/// it, and so converts into [`RpcError`].
pub trait ErrorCode: std::error::Error {
    fn code(&self) -> &'static str;

    fn detail(&self) -> Option<String> {
        None
    }
}

impl<E: ErrorCode> From<E> for RpcError {
    fn from(error: E) -> Self {
        let code = error.code();
        if code == "internal_error" {
            // Its message may describe the daemon's internals.
            tracing::warn!(%error, "a method failed");
            return Self::internal();
        }
        Self::failed(code, error.to_string()).with_detail(error.detail())
    }
}

impl ErrorCode for crate::core::CoreError {
    fn code(&self) -> &'static str {
        crate::core::CoreError::code(self)
    }
}

/// Where a [`StreamMethod`]'s handler sends its items.
pub struct Items<T> {
    id: Value,
    out: mpsc::Sender<String>,
    _item: std::marker::PhantomData<fn(T)>,
}

impl<T: Serialize> Items<T> {
    /// Send `item` to the client; `false` once the connection is gone, when
    /// the handler should stop.
    pub async fn send(&self, item: &T) -> bool {
        let Ok(item) = serde_json::to_value(item) else {
            return false;
        };
        let message = Message::stream(self.id.clone(), item);
        self.out.send(message.to_line()).await.is_ok()
    }
}

type Handler =
    Arc<dyn Fn(Value, Value, mpsc::Sender<String>) -> BoxFuture<'static, Reply> + Send + Sync>;
type Reply = Result<Value, RpcError>;

/// The methods a server answers, by name.
#[derive(Clone, Default)]
pub struct Methods {
    handlers: HashMap<&'static str, Handler>,
}

impl Methods {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `M` with `handler`, which gets a clone of `state` (the
    /// core, or a plugin and its context) with each request.
    ///
    /// # Panics
    ///
    /// If `M` already has a handler: a build error every test would hit.
    pub fn add<S, M, F, Fut, E>(&mut self, state: S, handler: F)
    where
        S: Clone + Send + Sync + 'static,
        M: Method,
        F: Fn(S, M) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Output, E>> + Send + 'static,
        E: Into<RpcError>,
    {
        let handler = Arc::new(handler);
        self.insert(
            M::NAME,
            Arc::new(move |params, _id, _out| {
                let (handler, state) = (handler.clone(), state.clone());
                Box::pin(
                    async move { answer::<M, _>(params, |params| handler(state, params)).await },
                )
            }),
        );
    }

    /// Like [`Self::add`], for a method whose handler sends items through
    /// [`Items`] before it answers.
    ///
    /// # Panics
    ///
    /// As [`Self::add`].
    pub fn add_stream<S, M, F, Fut, E>(&mut self, state: S, handler: F)
    where
        S: Clone + Send + Sync + 'static,
        M: StreamMethod,
        F: Fn(S, M, Items<M::Item>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Output, E>> + Send + 'static,
        E: Into<RpcError>,
    {
        let handler = Arc::new(handler);
        self.insert(
            M::NAME,
            Arc::new(move |params, id, out| {
                let (handler, state) = (handler.clone(), state.clone());
                let items = Items {
                    id,
                    out,
                    _item: std::marker::PhantomData,
                };
                Box::pin(async move {
                    answer::<M, _>(params, |params| handler(state, params, items)).await
                })
            }),
        );
    }

    fn insert(&mut self, name: &'static str, handler: Handler) {
        if self.handlers.insert(name, handler).is_some() {
            panic!("two handlers for {name:?}");
        }
    }

    /// Every method's name, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.handlers.keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// Run the request `method(params)`, sending stream items for request
    /// `id` into `out`.
    pub(crate) fn call(
        &self,
        method: &str,
        params: Value,
        id: Value,
        out: mpsc::Sender<String>,
    ) -> BoxFuture<'static, Reply> {
        match self.handlers.get(method) {
            Some(handler) => handler(params, id, out),
            None => {
                let error = RpcError::new(
                    code::METHOD_NOT_FOUND,
                    "method_not_found",
                    format!("no method {method:?}"),
                );
                Box::pin(async move { Err(error) })
            }
        }
    }
}

/// Decode `params` as `M`, run it, and encode its output.
async fn answer<M, Fut>(params: Value, run: impl FnOnce(M) -> Fut) -> Reply
where
    M: Method,
    Fut: Future,
    Fut::Output: IntoReply<M::Output>,
{
    // A method without fields may be sent without params.
    let params = if params.is_null() {
        Value::Object(Default::default())
    } else {
        params
    };
    let params: M = serde_json::from_value(params)
        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
    let output = run(params).await.into_reply()?;
    serde_json::to_value(output).map_err(|_| RpcError::internal())
}

trait IntoReply<T> {
    fn into_reply(self) -> Result<T, RpcError>;
}

impl<T, E: Into<RpcError>> IntoReply<T> for Result<T, E> {
    fn into_reply(self) -> Result<T, RpcError> {
        self.map_err(Into::into)
    }
}

/// A message on the wire, either way: a request (`method` and `id`), a
/// notification (`method` without `id`), or an answer (`id` with `result`
/// or `error`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Message {
    pub jsonrpc: Version,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// The `stream` notification's params.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StreamItem {
    pub id: Value,
    pub item: Value,
}

/// The notification method that carries a [`StreamMethod`]'s items.
pub(crate) const STREAM: &str = "stream";

impl Message {
    pub fn request(id: u64, method: &str, params: Value) -> Self {
        Self {
            id: Some(id.into()),
            method: Some(method.to_owned()),
            params: Some(params),
            ..Self::default()
        }
    }

    pub fn answer(id: Value, reply: Reply) -> Self {
        let (result, error) = match reply {
            Ok(result) => (Some(result), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            id: Some(id),
            result,
            error,
            ..Self::default()
        }
    }

    fn stream(id: Value, item: Value) -> Self {
        Self {
            method: Some(STREAM.to_owned()),
            params: Some(
                serde_json::to_value(StreamItem { id, item }).expect("a stream item serializes"),
            ),
            ..Self::default()
        }
    }

    /// The message as a line, newline included.
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("a message serializes");
        line.push('\n');
        line
    }
}

/// Always `"2.0"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = String::deserialize(deserializer)?;
        if version == "2.0" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("jsonrpc must be \"2.0\""))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Echo {
        text: String,
    }

    impl Method for Echo {
        const NAME: &'static str = "test.echo";
        type Output = String;
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Fail {}

    impl Method for Fail {
        const NAME: &'static str = "test.fail";
        type Output = ();
    }

    fn methods() -> Methods {
        let mut methods = Methods::new();
        methods.add(
            (),
            |(), echo: Echo| async move { Ok::<_, RpcError>(echo.text) },
        );
        methods.add((), |(), _: Fail| async {
            Err::<(), _>(crate::core::CoreError::NotPaired)
        });
        methods
    }

    async fn call(method: &str, params: Value) -> Reply {
        let (out, _) = mpsc::channel(1);
        methods().call(method, params, Value::Null, out).await
    }

    #[tokio::test]
    async fn a_method_answers_from_its_params() {
        let reply = call("test.echo", serde_json::json!({"text": "hi"})).await;
        assert_eq!(reply, Ok(Value::from("hi")));
    }

    #[tokio::test]
    async fn bad_params_and_unknown_methods_get_jsonrpc_codes() {
        let reply = call("test.echo", serde_json::json!({"txt": "hi"})).await;
        assert_eq!(reply.unwrap_err().code, code::INVALID_PARAMS);
        let reply = call("test.nope", Value::Null).await;
        assert_eq!(reply.unwrap_err().code, code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_method_without_fields_takes_no_params() {
        let error = call("test.fail", Value::Null).await.unwrap_err();
        assert_eq!(error.code, code::FAILED);
        assert_eq!(error.error_code(), "device_not_paired");
        assert_eq!(error.message, "device is not paired");
    }

    #[test]
    #[should_panic(expected = "two handlers")]
    fn a_method_has_one_handler() {
        let mut methods = methods();
        methods.add(
            (),
            |(), echo: Echo| async move { Ok::<_, RpcError>(echo.text) },
        );
    }

    #[test]
    fn an_answer_always_has_a_result_or_an_error() {
        let line = Message::answer(1.into(), Ok(Value::Null)).to_line();
        assert_eq!(line, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}\n");
        let line = Message::answer(1.into(), Err(RpcError::internal())).to_line();
        assert!(line.contains("\"error\"") && !line.contains("\"result\""));
    }
}
