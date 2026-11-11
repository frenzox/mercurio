//! Posts a small JSON document to a URL for every client event.

use std::sync::OnceLock;

use mercurio_plugin::{export_plugin, host, Config, Event, Hooks, Level, Plugin, Registration};
use wasip2::http::{
    outgoing_handler,
    types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme},
};

/// Set by `init`; settings never change, so each copy keeps its own.
static URL: OnceLock<String> = OnceLock::new();

struct WebhookAudit;

impl Plugin for WebhookAudit {
    fn init(config: Config) -> Result<Registration, String> {
        let _ = URL.set(config.require("url")?.to_string());
        Ok(Registration {
            hooks: Hooks::ON_EVENT,
            auth_methods: Vec::new(),
        })
    }

    fn on_event(event: Event) {
        let Some(url) = URL.get() else {
            return;
        };
        let body = match event {
            Event::Connected(client) => object("connected", &client.client_id),
            Event::Disconnected(gone) => object("disconnected", &gone.client.client_id),
            Event::Subscribed(sub) => object("subscribed", &sub.client.client_id),
            Event::Unsubscribed(sub) => object("unsubscribed", &sub.client.client_id),
            Event::SessionExpired(client_id) => object("session-expired", &client_id),
        };
        if let Err(error) = post(url, body.as_bytes()) {
            host::log(Level::Warn, &format!("posting to {url} failed: {error}"));
        }
    }
}

fn object(event: &str, client: &str) -> String {
    format!(
        r#"{{"event":"{}","client":"{}"}}"#,
        escape(event),
        escape(client)
    )
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Parses the URL minimally: `http://authority/path`, without a bare query.
fn post(url: &str, body: &[u8]) -> Result<(), String> {
    let rest = url.strip_prefix("http://").ok_or("http:// only")?;
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_string()), |(a, p)| (a, format!("/{p}")));
    let headers = Fields::from_list(&[("content-type".to_string(), b"application/json".to_vec())])
        .map_err(|e| format!("headers: {e:?}"))?;
    let request = OutgoingRequest::new(headers);
    request
        .set_method(&Method::Post)
        .map_err(|()| "invalid method")?;
    request
        .set_scheme(Some(&Scheme::Http))
        .map_err(|()| "invalid scheme")?;
    request
        .set_authority(Some(authority))
        .map_err(|()| "invalid authority")?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|()| "invalid path")?;
    let out = request.body().map_err(|()| "no request body")?;
    {
        let stream = out.write().map_err(|()| "no body stream")?;
        stream
            .blocking_write_and_flush(body)
            .map_err(|e| format!("write: {e:?}"))?;
    }
    let future = outgoing_handler::handle(request, None).map_err(|e| format!("handle: {e:?}"))?;
    OutgoingBody::finish(out, None).map_err(|e| format!("finish: {e:?}"))?;
    future.subscribe().block();
    let response = future
        .get()
        .ok_or("no response")?
        .map_err(|()| "response taken")?
        .map_err(|e| format!("response: {e:?}"))?;
    let status = response.status();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("status {status}"))
    }
}

export_plugin!(WebhookAudit);
