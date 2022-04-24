use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use rand::{self, prelude::SliceRandom};
use tokio::sync::broadcast;

use crate::{message::Message, topic_tree::TopicTree};

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
    shared_subscriptions: HashMap<String, HashMap<String, Vec<broadcast::Sender<Message>>>>,
}

fn get_random_element<T>(vec: &[T]) -> Option<&T> {
    let mut rng = rand::thread_rng();
    vec.choose(&mut rng)
}

const CHANNEL_SIZE: usize = 5;

impl Broker {
    pub(crate) fn new() -> Broker {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                subscriptions: TopicTree::new(),
                shared_subscriptions: HashMap::new(),
            }),
        });

        Broker { shared }
    }

    pub(crate) fn subscribe(&self, topic: String) -> broadcast::Receiver<Message> {
        let mut state = self.shared.state.lock().unwrap();
        state.subscriptions.subscribe(topic)
    }

    pub(crate) fn subscribe_shared(
        &self,
        topic: String,
        share_name: String,
    ) -> broadcast::Receiver<Message> {
        let (tx, rx) = broadcast::channel(CHANNEL_SIZE);
        let mut state = self.shared.state.lock().unwrap();

        let inner = state
            .shared_subscriptions
            .entry(topic)
            .or_insert_with(HashMap::<String, Vec<broadcast::Sender<Message>>>::new);

        match inner.get_mut(&share_name) {
            Some(v) => v.push(tx),
            None => {
                let subs: Vec<broadcast::Sender<Message>> = vec![tx];
                inner.insert(share_name, subs);
            }
        };

        rx
    }

    pub(crate) fn publish(&self, topic: &str, message: Message) -> crate::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        state.subscriptions.publish(topic, message);

        Ok(())
    }
}
