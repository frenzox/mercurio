//! Sending one command to the broker and waiting for its answer.

use std::{fmt, io, path::Path, time::Duration};

use mercurio_client::Bytes;
use mercurio_client::{cli::ConnectArgs, ClientError, Event, MessageProperties, MqttClient, QoS};
use serde_json::{json, Value};

use crate::commands::{Request, DYNSEC_TOPIC};

/// Why mercurioctl failed, which sets its exit status.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// The broker refused the command, with its error code when it gave one
    Rejected {
        code: Option<String>,
        message: String,
    },
    /// The broker couldn't be reached, refused the login or didn't answer
    /// in time
    Connection(String),
    /// The command line asks for something that can't be done
    Usage(String),
}

impl Failure {
    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::Rejected { .. } => 1,
            Failure::Connection(_) => 2,
            Failure::Usage(_) => 3,
        }
    }

    /// The failure as `--json` prints it: the message and the broker's
    /// error code, null when there is none.
    pub fn to_json(&self) -> Value {
        let (code, message) = match self {
            Failure::Rejected { code, message } => (code.as_deref(), message),
            Failure::Connection(message) | Failure::Usage(message) => (None, message),
        };
        json!({ "error": message, "code": code })
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Rejected {
                code: Some(code),
                message,
            } => write!(f, "{message} ({code})"),
            Failure::Rejected {
                code: None,
                message,
            }
            | Failure::Connection(message)
            | Failure::Usage(message) => f.write_str(message),
        }
    }
}

/// Where mercuriod serves its admin socket by default.
pub const ADMIN_SOCKET: &str = "/run/mercurio/admin.sock";

/// With no connection option given, the broker's admin socket when it
/// exists, which needs no credentials on the broker's host; otherwise what
/// the options say, `localhost:1883` when they say nothing.
pub fn resolve(mut args: ConnectArgs, admin_socket: &Path) -> ConnectArgs {
    #[cfg(unix)]
    {
        let none_given = args.host.is_none()
            && args.port.is_none()
            && args.unix.is_none()
            && args.username.is_none()
            && args.password.is_none()
            && !args.tls
            && args.cafile.is_none()
            && !args.insecure
            && args.cert.is_none()
            && !args.ws;
        if none_given && admin_socket.exists() {
            args.unix = Some(admin_socket.to_path_buf());
        }
    }
    #[cfg(not(unix))]
    let _ = admin_socket;
    args
}

/// Sends `request` to the broker `args` name and waits up to `wait`,
/// connecting included, for its answer: the response's data, if any.
pub async fn send(
    args: &ConnectArgs,
    request: &Request,
    wait: Duration,
) -> Result<Option<Value>, Failure> {
    let outcome = tokio::time::timeout(wait, async {
        let client = MqttClient::connect(args.to_connect_options())
            .await
            .map_err(|e| connection_failure(&target(args), e))?;
        let result = ask(&client, request).await;
        Ok::<_, Failure>((client, result))
    })
    .await;
    match outcome {
        Ok(Ok((client, result))) => {
            // The answer is in; a slow goodbye must not change it
            let _ = tokio::time::timeout(Duration::from_secs(1), client.disconnect()).await;
            result
        }
        Ok(Err(failure)) => Err(failure),
        Err(_) => {
            let mut message = format!("no answer from the broker within {} s", wait.as_secs());
            // Nothing answers its topic on a broker without it
            if request.topic == DYNSEC_TOPIC {
                message.push_str("; is dynamic security enabled?");
            } else {
                message.push_str("; is it a Mercurio broker that serves $CONTROL/broker/v1?");
            }
            Err(Failure::Connection(message))
        }
    }
}

/// Connecting to `target` failed with `error`.
fn connection_failure(target: &str, error: ClientError) -> Failure {
    let mut message = format!("{target}: {error}");
    if matches!(&error, ClientError::ConnectionFailed(e) if e.kind() == io::ErrorKind::PermissionDenied)
    {
        message.push_str(
            "; run as root or a member of the mercurio group, or use -H to connect over the \
             network",
        );
    }
    Failure::Connection(message)
}

/// Where `args` connect to, for messages.
fn target(args: &ConnectArgs) -> String {
    #[cfg(unix)]
    if let Some(path) = &args.unix {
        return path.display().to_string();
    }
    format!(
        "{}:{}",
        args.host.as_deref().unwrap_or("localhost"),
        args.port.unwrap_or(1883)
    )
}

async fn ask(client: &MqttClient, request: &Request) -> Result<Option<Value>, Failure> {
    let response_topic = response_topic(request.topic, &token());
    let correlation = Bytes::from(token());
    let granted = client
        .subscribe(&[(response_topic.as_str(), QoS::AtLeastOnce)])
        .await
        .map_err(|e| Failure::Connection(e.to_string()))?;
    if granted.iter().any(|result| !result.success) {
        return Err(Failure::Rejected {
            code: None,
            message: format!("not allowed to subscribe to {response_topic}"),
        });
    }
    let properties = MessageProperties::default()
        .response_topic(response_topic.clone())
        .correlation_data(correlation.clone());
    let payload = json!({ "commands": [&request.command] }).to_string();
    // QoS 1, so a request the broker refuses fails here instead of going
    // unanswered
    client
        .publish_with_properties(request.topic, payload, QoS::AtLeastOnce, false, properties)
        .await
        .map_err(|e| match e {
            ClientError::PublishRefused(reason) => Failure::Rejected {
                code: None,
                message: format!(
                    "the broker refused the request ({})",
                    camel_case(&format!("{reason:?}"))
                ),
            },
            e => Failure::Connection(e.to_string()),
        })?;
    loop {
        match client.recv().await {
            Some(Event::Message {
                topic,
                payload,
                properties,
                ..
            }) if topic == response_topic
                && properties.correlation_data.as_ref() == Some(&correlation) =>
            {
                return answer(&payload);
            }
            Some(Event::Disconnected { reason }) => {
                return Err(Failure::Connection(format!(
                    "the broker closed the connection: {reason:?}"
                )));
            }
            Some(_) => {}
            None => return Err(Failure::Connection("the connection closed".to_string())),
        }
    }
}

/// The data of the one response in `payload`, or why its command failed.
pub fn answer(payload: &[u8]) -> Result<Option<Value>, Failure> {
    let envelope: Value = serde_json::from_slice(payload)
        .map_err(|e| Failure::Connection(format!("unreadable answer: {e}")))?;
    let response = envelope["responses"]
        .get(0)
        .ok_or_else(|| Failure::Connection("an answer without a response".to_string()))?;
    if let Some(error) = response.get("error") {
        return Err(Failure::Rejected {
            code: response["code"].as_str().map(str::to_string),
            message: error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_string),
        });
    }
    Ok(response.get("data").cloned())
}

/// Under the request's own topic, which any administrator allowed to send
/// the request may also subscribe to: dynamic security's older administrator
/// roles grant `$CONTROL/dynamic-security/#` only. The broker consumes
/// publishes there without routing them to subscribers.
fn response_topic(request_topic: &str, token: &str) -> String {
    format!("{request_topic}/response/{token}")
}

/// `NotAuthorized` as `notAuthorized`, how command errors spell their codes.
fn camel_case(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_lowercase().chain(chars).collect())
        .unwrap_or_default()
}

/// 16 hex digits another run is unlikely to pick: `RandomState`'s keys are
/// seeded from the operating system for each process.
fn token() -> String {
    use std::hash::BuildHasher;

    let random = std::collections::hash_map::RandomState::new();
    format!("{:016x}", random.hash_one(std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;
    use serde_json::json;

    use super::{answer, camel_case, connection_failure, resolve, response_topic, token, Failure};
    use crate::cli::Cli;

    #[test]
    fn a_socket_refused_for_permissions_says_how_to_get_in() {
        use mercurio_client::ClientError;
        use std::io::{Error, ErrorKind};

        // The message of the I/O error doesn't matter, only its kind
        let denied =
            ClientError::ConnectionFailed(Error::new(ErrorKind::PermissionDenied, "EACCES"));
        let Failure::Connection(message) = connection_failure("/run/a.sock", denied) else {
            panic!("not a connection failure");
        };
        assert!(message.starts_with("/run/a.sock: "), "{message}");
        assert!(
            message.contains("member of the mercurio group"),
            "{message}"
        );

        let refused = ClientError::ConnectionFailed(Error::new(
            ErrorKind::ConnectionRefused,
            "Permission denied in the text only",
        ));
        let Failure::Connection(message) = connection_failure("/run/a.sock", refused) else {
            panic!("not a connection failure");
        };
        assert!(!message.contains("mercurio group"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn the_admin_socket_is_used_when_no_connection_option_is_given() {
        let connect = |options: &[&str]| {
            let argv = std::iter::once("mercurioctl")
                .chain(options.iter().copied())
                .chain(["broker", "info"]);
            Cli::try_parse_from(argv).unwrap().connect
        };
        // Any path that exists will do
        let socket = std::env::temp_dir();

        assert_eq!(
            resolve(connect(&[]), &socket).unix.as_deref(),
            Some(socket.as_path())
        );
        assert_eq!(
            resolve(connect(&[]), Path::new("/nonexistent/admin.sock")).unix,
            None
        );
        // -i alone still uses the socket
        assert!(resolve(connect(&["-i", "me"]), &socket).unix.is_some());
        for options in [
            &["-H", "broker"][..],
            &["-p", "1884"],
            &["-u", "alice"],
            &["-P", "secret"],
            &["--tls"],
            &["--cafile", "ca.pem"],
            &["--insecure"],
            &["--cert", "c.pem", "--key", "c.key"],
            &["--ws"],
        ] {
            assert_eq!(resolve(connect(options), &socket).unix, None, "{options:?}");
        }
        assert_eq!(
            resolve(connect(&["--unix", "/elsewhere.sock"]), &socket)
                .unix
                .as_deref(),
            Some(Path::new("/elsewhere.sock"))
        );
    }

    #[test]
    fn an_answer_gives_the_data_or_why_the_command_failed() {
        assert_eq!(
            answer(br#"{"responses":[{"command":"getLogLevel","data":{"level":"info"}}]}"#),
            Ok(Some(json!({"level": "info"})))
        );
        assert_eq!(
            answer(br#"{"responses":[{"command":"setLogLevel"}]}"#),
            Ok(None)
        );
        assert_eq!(
            answer(
                br#"{"responses":[{"command":"getClient","error":"no client `x`","code":"notFound"}]}"#
            ),
            Err(Failure::Rejected {
                code: Some("notFound".to_string()),
                message: "no client `x`".to_string(),
            })
        );
        // Dynamic security's errors have no code
        assert_eq!(
            answer(br#"{"responses":[{"command":"getRole","error":"Role not found"}]}"#),
            Err(Failure::Rejected {
                code: None,
                message: "Role not found".to_string(),
            })
        );
        assert_eq!(
            answer(br#"{"responses":[{"command":"x","error":{"why":1}}]}"#),
            Err(Failure::Rejected {
                code: None,
                message: r#"{"why":1}"#.to_string(),
            })
        );
        assert!(matches!(answer(b"nonsense"), Err(Failure::Connection(_))));
        assert!(matches!(
            answer(br#"{"responses":[]}"#),
            Err(Failure::Connection(_))
        ));
    }

    #[test]
    fn failures_print_their_code_and_set_the_exit_status() {
        let rejected = Failure::Rejected {
            code: Some("notFound".to_string()),
            message: "no client `x`".to_string(),
        };
        assert_eq!(rejected.to_string(), "no client `x` (notFound)");
        assert_eq!(
            rejected.to_json(),
            json!({ "error": "no client `x`", "code": "notFound" })
        );
        assert_eq!(
            Failure::Usage("no".to_string()).to_json(),
            json!({ "error": "no", "code": null })
        );
        assert_eq!(rejected.exit_code(), 1);
        assert_eq!(Failure::Connection("refused".to_string()).exit_code(), 2);
        assert_eq!(Failure::Usage("no".to_string()).exit_code(), 3);
    }

    #[test]
    fn responses_come_back_under_the_request_topic() {
        use crate::commands::{BROKER_TOPIC, DYNSEC_TOPIC};

        assert_eq!(
            response_topic(BROKER_TOPIC, "ab"),
            "$CONTROL/broker/v1/response/ab"
        );
        assert_eq!(
            response_topic(DYNSEC_TOPIC, "ab"),
            "$CONTROL/dynamic-security/v1/response/ab"
        );
    }

    #[test]
    fn reason_codes_are_spelled_like_command_errors() {
        assert_eq!(camel_case("NotAuthorized"), "notAuthorized");
        assert_eq!(camel_case(""), "");
    }

    #[test]
    fn tokens_are_sixteen_hex_digits_and_differ() {
        let (a, b) = (token(), token());
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }
}
