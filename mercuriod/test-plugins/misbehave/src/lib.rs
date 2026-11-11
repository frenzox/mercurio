//! A plugin that abuses everything the sandbox limits, for the host's tests.

use std::time::Duration;

use mercurio_plugin::{
    export_plugin, host, AuthResult, ClientInfo, Decision, Hooks, Message, MessageAction, Plugin,
    Qos, Registration, Success,
};
use wasip2::{
    http::{
        outgoing_handler,
        types::{Fields, Method, OutgoingRequest, Scheme},
    },
    sockets::{network::IpAddressFamily, tcp_create_socket::create_tcp_socket},
};

struct Misbehave;

impl Plugin for Misbehave {
    fn init(config: Vec<(String, String)>) -> Result<Registration, String> {
        let sleep = config
            .iter()
            .find(|(key, _)| key == "init_sleep_ms")
            .and_then(|(_, value)| value.parse().ok());
        if let Some(ms) = sleep {
            std::thread::sleep(Duration::from_millis(ms));
        }
        Ok(Registration {
            hooks: Hooks::ON_CONTROL | Hooks::AUTHORIZE_CONTROL | Hooks::ON_MESSAGE,
            auth_methods: vec!["WHO".into()],
        })
    }

    /// Succeeds as the client it was asked about, sending the data it got.
    fn auth_step(
        client: ClientInfo,
        _method: String,
        _state: Option<Vec<u8>>,
        data: Option<Vec<u8>>,
    ) -> AuthResult {
        AuthResult::Success(Success {
            username: Some(format!("{} {:?}", client.client_id, client.username)),
            data,
        })
    }

    /// Lets user `admin` administer, and leaves everyone else to the chain.
    fn authorize_control(client: ClientInfo, _topic: String) -> Decision {
        match client.username.as_deref() {
            Some("admin") => Decision::Allow,
            _ => Decision::Defer,
        }
    }

    /// Publishes a copy of each message under `echo/` one level deeper,
    /// which would never end if it saw its own copies.
    fn on_message(_client: ClientInfo, message: Message) -> MessageAction {
        if message.topic.starts_with("echo/") {
            let topic = format!("echo/{}", message.topic);
            let _ = host::publish(&Message { topic, ..message });
        }
        MessageAction::Keep
    }

    fn on_control(client: ClientInfo, path: String, payload: Vec<u8>) -> Option<Vec<u8>> {
        let text = String::from_utf8_lossy(&payload).into_owned();
        match path.as_str() {
            "client" => Some(format!("{client:?}").into_bytes()),
            "publish" => {
                let (topic, payload) = text.split_once('\n').unwrap_or((&text, ""));
                let mut message = message(topic, payload.as_bytes().to_vec());
                message.content_type = Some("text/plain".into());
                message.user_properties = vec![("by".into(), "misbehave".into())];
                Some(report(host::publish(&message)))
            }
            "publish-big" => {
                let size: usize = text.parse().ok()?;
                Some(report(host::publish(&message("big", vec![0; size]))))
            }
            "publish-flood" => {
                let most: usize = text.parse().ok()?;
                let mut published = 0;
                let error = loop {
                    if published == most {
                        break "none".to_string();
                    }
                    match host::publish(&message(&format!("flood/{published}"), Vec::new())) {
                        Ok(()) => published += 1,
                        Err(error) => break error,
                    }
                };
                Some(format!("{published} {error}").into_bytes())
            }
            "publish-spin" => loop {
                let _ = host::publish(&message("spin", Vec::new()));
            },
            "panic" => panic!("misbehaving on purpose: {text}"),
            "spin" => loop {
                std::hint::spin_loop()
            },
            "sleep" => {
                std::thread::sleep(Duration::from_secs(60));
                None
            }
            "grow" => {
                let mb: usize = text.parse().ok()?;
                let buffer = vec![1u8; mb << 20];
                Some(std::hint::black_box(&buffer).len().to_string().into_bytes())
            }
            "http" => Some(report(http_get(&text))),
            "tcp" => Some(report(
                std::net::TcpStream::connect(text.as_str())
                    .map(drop)
                    .map_err(|e| format!("{e} ({:?})", e.kind())),
            )),
            "listen" => Some(report(
                std::net::TcpListener::bind(text.as_str())
                    .map(drop)
                    .map_err(|e| format!("{e} ({:?})", e.kind())),
            )),
            "sockets" => {
                let most: usize = text.parse().ok()?;
                let mut held = Vec::new();
                let error = loop {
                    if held.len() == most {
                        break "none".to_string();
                    }
                    match create_tcp_socket(IpAddressFamily::Ipv4) {
                        Ok(socket) => held.push(socket),
                        Err(error) => break format!("{error:?}"),
                    }
                };
                let count = held.len();
                // Kept open, so the copy goes back to the pool holding them
                std::mem::forget(held);
                Some(format!("{count} {error}").into_bytes())
            }
            "fields" => {
                let most: usize = text.parse().ok()?;
                let held: Vec<_> = (0..most).map(|_| Fields::new()).collect();
                std::mem::forget(held);
                Some(most.to_string().into_bytes())
            }
            "kv-mark" => {
                host::kv_set(&format!("mark/{text}"), b"").ok()?;
                Some(Vec::new())
            }
            "kv-marks" => Some(host::kv_list("mark/").len().to_string().into_bytes()),
            "kv-count" => {
                let count = host::kv_get("count")
                    .and_then(|v| String::from_utf8(v).ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
                    + 1;
                host::kv_set("count", count.to_string().as_bytes()).ok()?;
                Some(count.to_string().into_bytes())
            }
            _ => None,
        }
    }
}

fn message(topic: &str, payload: Vec<u8>) -> Message {
    Message {
        topic: topic.into(),
        payload,
        qos: Qos::AtLeastOnce,
        retain: false,
        payload_is_utf8: false,
        message_expiry: None,
        content_type: None,
        response_topic: None,
        correlation_data: None,
        user_properties: Vec::new(),
    }
}

fn report(result: Result<(), String>) -> Vec<u8> {
    result.map_or_else(String::into_bytes, |()| b"ok".to_vec())
}

fn http_get(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("http://").ok_or("http:// only")?;
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_string()), |(a, p)| (a, format!("/{p}")));
    let request = OutgoingRequest::new(Fields::new());
    request.set_method(&Method::Get).unwrap();
    request.set_scheme(Some(&Scheme::Http)).unwrap();
    request.set_authority(Some(authority)).unwrap();
    request.set_path_with_query(Some(&path)).unwrap();
    let future = outgoing_handler::handle(request, None).map_err(|e| format!("handle: {e:?}"))?;
    future.subscribe().block();
    future
        .get()
        .unwrap()
        .unwrap()
        .map(drop)
        .map_err(|e| format!("response: {e:?}"))
}

export_plugin!(Misbehave);
