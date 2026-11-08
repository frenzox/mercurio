//! Topic names and filters come from clients; checking them must never
//! panic, and a valid name matches a filter that is itself.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mercurio_core::topic::{
    parse_shared_subscription, topic_matches_filter, validate_publish_topic,
    validate_subscribe_filter,
};

fuzz_target!(|data: (&str, &str)| {
    let (topic, filter) = data;
    let topic_valid = validate_publish_topic(topic).is_ok();
    let filter_valid = validate_subscribe_filter(filter).is_ok();
    let _ = parse_shared_subscription(filter);
    if topic_valid && filter_valid {
        let _ = topic_matches_filter(topic, filter);
    }
    if topic_valid {
        assert!(topic_matches_filter(topic, topic));
    }
});
