//! `$CONTROL/broker/v1`: the JSON envelope around [`Admin`], with each
//! command allowed on its own.

use bytes::Bytes;
use mercurio_core::{codec::Decoder, reason::ReasonCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{Admin, AdminError, ClientQuery, DisconnectOptions, ErrorCode, Page, Paging};
use crate::{
    plugin::{Access, Chain, ClientInfo, Decision},
    server::AddressRange,
};

/// Served by `mercuriod` through its `AdminExtension`.
const DAEMON_COMMANDS: [&str; 5] = [
    "listBridges",
    "getConfig",
    "getLogLevel",
    "setLogLevel",
    "reload",
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisconnectArgs {
    clientid: String,
    send_will: Option<bool>,
    reason_code: Option<u8>,
}

#[derive(Deserialize)]
struct DeleteSessionArgs {
    clientid: String,
    disconnect: Option<bool>,
}

fn reason_code(code: u8) -> Result<ReasonCode, AdminError> {
    ReasonCode::decode(&mut &[code][..])
        .map_err(|_| AdminError::invalid_argument(format!("0x{code:02x} is not a reason code")))
}

#[derive(Deserialize)]
struct Envelope {
    commands: Vec<Value>,
}

#[derive(Serialize)]
struct Responses {
    responses: Vec<Response>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    correlation_data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<ErrorCode>,
}

impl Response {
    fn new(
        command: String,
        correlation_data: Option<Value>,
        result: Result<Value, AdminError>,
    ) -> Self {
        let (data, error, code) = match result {
            Ok(Value::Null) => (None, None, None),
            Ok(data) => (Some(data), None, None),
            Err(e) => (None, Some(e.message), Some(e.code)),
        };
        Response {
            command,
            correlation_data,
            data,
            error,
            code,
        }
    }
}

#[derive(Deserialize)]
struct PageArgs {
    offset: Option<usize>,
    count: Option<usize>,
}

impl PageArgs {
    fn paging(&self) -> Result<Paging, AdminError> {
        let default = Paging::default();
        Paging::new(
            self.offset.unwrap_or(default.offset),
            self.count.unwrap_or(default.count),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListClientsArgs {
    connected: Option<bool>,
    clientid_prefix: Option<String>,
    username: Option<String>,
    address: Option<String>,
    #[serde(flatten)]
    page: PageArgs,
}

impl ListClientsArgs {
    fn query(self) -> Result<ClientQuery, AdminError> {
        let address = self
            .address
            .map(|a| a.parse::<AddressRange>())
            .transpose()
            .map_err(AdminError::invalid_argument)?;
        Ok(ClientQuery {
            connected: self.connected,
            client_id_prefix: self.clientid_prefix,
            username: self.username,
            address,
            paging: self.page.paging()?,
        })
    }
}

#[derive(Deserialize)]
struct ClientArgs {
    clientid: String,
}

/// Runs the commands `client` sent in `payload`, in order, and returns the
/// response payload.
pub(crate) async fn handle(
    admin: &Admin,
    chain: &Chain,
    client: &ClientInfo,
    payload: &[u8],
) -> Bytes {
    let responses = match serde_json::from_slice::<Envelope>(payload) {
        Ok(envelope) => {
            let mut responses = Vec::with_capacity(envelope.commands.len());
            for request in envelope.commands {
                let correlation_data = request.get("correlationData").cloned();
                let command = request
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let result = run(admin, chain, client, &command, &request).await;
                responses.push(Response::new(command, correlation_data, result));
            }
            responses
        }
        Err(e) => vec![Response::new(
            String::new(),
            None,
            Err(AdminError::invalid_argument(format!(
                "not a command envelope: {e}"
            ))),
        )],
    };
    serde_json::to_vec(&Responses { responses })
        .expect("responses are plain JSON")
        .into()
}

#[derive(Deserialize)]
struct ListRetainedArgs {
    filter: Option<String>,
    #[serde(flatten)]
    page: PageArgs,
}

#[derive(Deserialize)]
struct TopicArgs {
    topic: String,
}

#[derive(Deserialize)]
struct FilterArgs {
    filter: String,
}

async fn run(
    admin: &Admin,
    chain: &Chain,
    client: &ClientInfo,
    command: &str,
    args: &Value,
) -> Result<Value, AdminError> {
    // The name becomes a topic level, so it can't carry `/`, `+` or `#`
    if command.is_empty() || !command.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(AdminError::invalid_argument(
            "`command` must name a command",
        ));
    }
    // A trusted chain allows every topic
    let topic = format!("$CONTROL/broker/v1/{command}");
    if chain.authorize(client, &topic, Access::Write).await != Decision::Allow {
        return Err(AdminError::new(
            ErrorCode::NotAuthorized,
            format!("not allowed to run {command}"),
        ));
    }
    Ok(match command {
        "getBrokerInfo" => json(admin.info().await),
        "listListeners" => {
            let paging = arguments::<PageArgs>(args)?.paging()?;
            listing("listeners", Page::of(admin.listeners().await, paging))
        }
        "listClients" => {
            let query = arguments::<ListClientsArgs>(args)?.query()?;
            listing("clients", admin.clients(&query).await)
        }
        "getClient" => json(
            admin
                .client(&arguments::<ClientArgs>(args)?.clientid)
                .await?,
        ),
        "disconnectClient" => {
            let a: DisconnectArgs = arguments(args)?;
            let mut options = DisconnectOptions::default();
            options.send_will = a.send_will.unwrap_or(options.send_will);
            if let Some(code) = a.reason_code {
                options.reason = reason_code(code)?;
            }
            admin.disconnect(&a.clientid, options).await?;
            Value::Null
        }
        "deleteSession" => {
            let a: DeleteSessionArgs = arguments(args)?;
            // The request runs in the client's own connection, which can't
            // close while it waits for itself to
            if a.clientid == client.client_id {
                return Err(AdminError::new(
                    ErrorCode::Conflict,
                    "a client can't delete its own session",
                ));
            }
            admin
                .delete_session(&a.clientid, a.disconnect.unwrap_or(false))
                .await?;
            Value::Null
        }
        "listRetained" => {
            let a: ListRetainedArgs = arguments(args)?;
            let filter = a.filter.as_deref().unwrap_or("#");
            listing("messages", admin.retained(filter, a.page.paging()?).await?)
        }
        "getRetained" => json(
            admin
                .retained_message(&arguments::<TopicArgs>(args)?.topic)
                .await?,
        ),
        "deleteRetained" => {
            let deleted = admin
                .delete_retained(&arguments::<FilterArgs>(args)?.filter)
                .await?;
            serde_json::json!({ "deleted": deleted })
        }
        _ => extension(admin, command, args).await?,
    })
}

async fn extension(admin: &Admin, command: &str, args: &Value) -> Result<Value, AdminError> {
    if let Some(extension) = admin.extension() {
        if let Some(result) = extension.handle(command, args).await {
            return result;
        }
    }
    Err(if DAEMON_COMMANDS.contains(&command) {
        AdminError::new(
            ErrorCode::Unavailable,
            format!("{command} is served by mercuriod"),
        )
    } else {
        AdminError::invalid_argument(format!("unknown command `{command}`"))
    })
}

fn arguments<T: DeserializeOwned>(args: &Value) -> Result<T, AdminError> {
    T::deserialize(args).map_err(|e| AdminError::invalid_argument(e.to_string()))
}

fn json(value: impl Serialize) -> Value {
    serde_json::to_value(value).expect("admin types are plain JSON")
}

/// A list command's data: the items under `key`, and `totalCount`.
fn listing<T: Serialize>(key: &str, page: Page<T>) -> Value {
    let mut object = Map::new();
    object.insert(key.to_string(), json(page.items));
    object.insert("totalCount".to_string(), page.total_count.into());
    Value::Object(object)
}
