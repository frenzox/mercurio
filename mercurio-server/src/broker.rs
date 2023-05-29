use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::topic_tree::TopicTree;
use mercurio_core::{message::Message, Result};

#[derive(Debug, Clone)]
pub(crate) struct Broker {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    subscriptions: TopicTree<Message>,
}

impl Broker {
    pub(crate) fn new() -> Broker {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                subscriptions: TopicTree::new(),
            }),
        });

        Broker { shared }
    }

    pub(crate) fn subscribe(&self, topic: String) -> broadcast::Receiver<Message> {
        let mut state = self.shared.state.lock().unwrap();
        state.subscriptions.subscribe(topic)
    }

    pub(crate) fn publish(&self, topic: &str, message: Message) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        state.subscriptions.publish(topic, message);

        Ok(())
    }
}
