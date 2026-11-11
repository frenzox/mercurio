//! Sending one command to the broker and waiting for its answer.

use std::{fmt, time::Duration};

use bytes::Bytes;
use mercurio_client::{cli::ConnectArgs, ClientError, Event, MessageProperties, MqttClient, QoS};
use serde_json::{json, Value};

use crate::commands::Request;

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
}

impl Failure {
    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::Rejected { .. } => 1,
            Failure::Connection(_) => 2,
        }
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
            | Failure::Connection(message) => f.write_str(message),
        }
    }
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
            .map_err(|e| Failure::Connection(format!("{}: {e}", target(args))))?;
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
        Err(_) => Err(Failure::Connection(format!(
            "no answer from the broker within {} s",
            wait.as_secs()
        ))),
    }
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
    // Under `$CONTROL/broker/v1/`, which the administrator `mercuriod
    // dynsec-init` creates may subscribe to
    let response_topic = format!("$CONTROL/broker/v1/response/{}", token());
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
    let properties = MessageProperties {
        response_topic: Some(response_topic.clone()),
        correlation_data: Some(correlation.clone()),
        ..Default::default()
    };
    let payload = json!({ "commands": [&request.command] }).to_string();
    // QoS 1, so a request the broker refuses fails here instead of going
    // unanswered
    client
        .publish_with_properties(request.topic, payload, QoS::AtLeastOnce, false, properties)
        .await
        .map_err(|e| match e {
            ClientError::PublishRefused(reason) => Failure::Rejected {
                code: None,
                message: format!("the broker refused the request: {reason:?}"),
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

/// 16 hex digits another run is unlikely to pick: `RandomState`'s keys are
/// seeded from the operating system for each process.
fn token() -> String {
    use std::hash::BuildHasher;

    let random = std::collections::hash_map::RandomState::new();
    format!("{:016x}", random.hash_one(std::process::id()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{answer, token, Failure};

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
        assert_eq!(rejected.exit_code(), 1);
        assert_eq!(Failure::Connection("refused".to_string()).exit_code(), 2);
    }

    #[test]
    fn tokens_are_sixteen_hex_digits_and_differ() {
        let (a, b) = (token(), token());
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }
}
