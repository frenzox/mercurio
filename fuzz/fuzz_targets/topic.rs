//! Topic names and filters come from clients; checking them must never
//! panic, and matching must agree with the rules of section 4.7: a filter
//! without wildcards matches only itself, `#` matches every topic not
//! starting with `$`, and a filter that covers another matches every topic
//! the other matches.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mercurio_core::topic::{
    filter_covers, parse_shared_subscription, topic_matches_filter, validate_publish_topic,
    validate_subscribe_filter,
};

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    topic: &'a str,
    filter: &'a str,
    subset: &'a str,
    /// Levels that stand in for the wildcards of `subset`
    levels: Vec<&'a str>,
}

/// A topic `filter` matches, with each `+` and `#` replaced by levels from
/// `levels`.
fn instantiate(filter: &str, levels: &[&str]) -> String {
    let mut levels = levels
        .iter()
        .map(|level| level.replace(['/', '+', '#'], ""));
    let mut topic = Vec::new();
    for level in filter.split('/') {
        match level {
            "+" => topic.push(levels.next().unwrap_or_default()),
            "#" => topic.extend(levels.by_ref().take(2)),
            level => topic.push(level.to_string()),
        }
    }
    topic.join("/")
}

fuzz_target!(|input: Input| {
    let Input {
        topic,
        filter,
        subset,
        levels,
    } = input;

    let topic_valid = validate_publish_topic(topic).is_ok();
    let filter_valid = validate_subscribe_filter(filter).is_ok();
    let _ = parse_shared_subscription(filter);
    let _ = topic_matches_filter(topic, filter);

    if !topic_valid {
        return;
    }
    assert!(topic_matches_filter(topic, topic));
    assert_eq!(topic_matches_filter(topic, "#"), !topic.starts_with('$'));

    if filter_valid && !filter.contains(['+', '#']) {
        assert_eq!(topic_matches_filter(topic, filter), topic == filter);
    }

    if !filter_valid || validate_subscribe_filter(subset).is_err() {
        return;
    }
    let subset_filter = parse_shared_subscription(subset).map_or(subset, |(_, filter)| filter);
    if !filter_covers(filter, subset) {
        return;
    }
    for candidate in [topic.to_string(), instantiate(subset_filter, &levels)] {
        if validate_publish_topic(&candidate).is_ok()
            && topic_matches_filter(&candidate, subset_filter)
        {
            assert!(
                topic_matches_filter(&candidate, filter),
                "{filter:?} covers {subset:?} but not its topic {candidate:?}"
            );
        }
    }
});
