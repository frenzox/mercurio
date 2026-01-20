//! MQTT topic validation utilities.
//!
//! This module provides validation for MQTT topic names and topic filters
//! according to the MQTT specification.

use core::fmt;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

/// Maximum topic name/filter length in bytes (UTF-8 encoded).
pub const MAX_TOPIC_LENGTH: usize = 65535;

/// Error type for topic validation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicValidationError {
    /// Topic is empty (zero length).
    Empty,
    /// Topic exceeds maximum length.
    TooLong,
    /// Topic contains null character (U+0000).
    ContainsNullChar,
    /// Wildcard characters not allowed in publish topics.
    WildcardInPublishTopic,
    /// Single-level wildcard (+) must occupy entire level.
    InvalidSingleLevelWildcard,
    /// Multi-level wildcard (#) must be at end and occupy entire level.
    InvalidMultiLevelWildcard,
}

impl fmt::Display for TopicValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TopicValidationError::Empty => write!(f, "topic name cannot be empty"),
            TopicValidationError::TooLong => {
                write!(
                    f,
                    "topic name exceeds maximum length of {} bytes",
                    MAX_TOPIC_LENGTH
                )
            }
            TopicValidationError::ContainsNullChar => {
                write!(f, "topic name cannot contain null character")
            }
            TopicValidationError::WildcardInPublishTopic => {
                write!(
                    f,
                    "wildcard characters (+, #) not allowed in publish topics"
                )
            }
            TopicValidationError::InvalidSingleLevelWildcard => {
                write!(
                    f,
                    "single-level wildcard (+) must occupy entire topic level"
                )
            }
            TopicValidationError::InvalidMultiLevelWildcard => {
                write!(
                    f,
                    "multi-level wildcard (#) must be at end and occupy entire level"
                )
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TopicValidationError {}

/// Validate a topic name for publishing.
///
/// Publish topic names must:
/// - Not be empty
/// - Not exceed 65535 bytes
/// - Not contain null characters
/// - Not contain wildcard characters (+ or #)
///
/// # Examples
///
/// ```
/// use mercurio_core::topic::validate_publish_topic;
///
/// assert!(validate_publish_topic("sensors/temperature/room1").is_ok());
/// assert!(validate_publish_topic("sensors/+/room1").is_err()); // wildcards not allowed
/// ```
pub fn validate_publish_topic(topic: &str) -> Result<(), TopicValidationError> {
    validate_common(topic)?;

    // Publish topics must not contain wildcards
    if topic.contains('+') || topic.contains('#') {
        return Err(TopicValidationError::WildcardInPublishTopic);
    }

    Ok(())
}

/// Validate a topic filter for subscribing.
///
/// Subscribe topic filters must:
/// - Not be empty
/// - Not exceed 65535 bytes
/// - Not contain null characters
/// - Single-level wildcard (+) must occupy entire level
/// - Multi-level wildcard (#) must be at end and occupy entire level
///
/// # Examples
///
/// ```
/// use mercurio_core::topic::validate_subscribe_filter;
///
/// // Valid filters
/// assert!(validate_subscribe_filter("sensors/temperature/room1").is_ok());
/// assert!(validate_subscribe_filter("sensors/+/room1").is_ok());
/// assert!(validate_subscribe_filter("sensors/#").is_ok());
/// assert!(validate_subscribe_filter("#").is_ok());
/// assert!(validate_subscribe_filter("+/+/+").is_ok());
///
/// // Invalid filters
/// assert!(validate_subscribe_filter("sensors/temp+/room1").is_err());
/// assert!(validate_subscribe_filter("sensors/#/room1").is_err());
/// ```
pub fn validate_subscribe_filter(filter: &str) -> Result<(), TopicValidationError> {
    validate_common(filter)?;

    let levels: Vec<&str> = filter.split('/').collect();

    for (i, level) in levels.iter().enumerate() {
        // Check single-level wildcard (+)
        if level.contains('+') && *level != "+" {
            // + must be the entire level, not mixed with other characters
            return Err(TopicValidationError::InvalidSingleLevelWildcard);
        }

        // Check multi-level wildcard (#)
        if level.contains('#') {
            // # must be the entire level
            if *level != "#" {
                return Err(TopicValidationError::InvalidMultiLevelWildcard);
            }
            // # must be the last level
            if i != levels.len() - 1 {
                return Err(TopicValidationError::InvalidMultiLevelWildcard);
            }
        }
    }

    Ok(())
}

/// Common validation rules for both publish topics and subscribe filters.
fn validate_common(topic: &str) -> Result<(), TopicValidationError> {
    // Check empty
    if topic.is_empty() {
        return Err(TopicValidationError::Empty);
    }

    // Check length (UTF-8 encoded bytes)
    if topic.len() > MAX_TOPIC_LENGTH {
        return Err(TopicValidationError::TooLong);
    }

    // Check for null character
    if topic.contains('\0') {
        return Err(TopicValidationError::ContainsNullChar);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_publish_topics() {
        assert!(validate_publish_topic("a").is_ok());
        assert!(validate_publish_topic("a/b/c").is_ok());
        assert!(validate_publish_topic("sensors/temperature/room1").is_ok());
        assert!(validate_publish_topic("/leading/slash").is_ok());
        assert!(validate_publish_topic("trailing/slash/").is_ok());
        assert!(validate_publish_topic("//double//slash").is_ok());
        assert!(validate_publish_topic("$SYS/broker/clients/connected").is_ok());
    }

    #[test]
    fn test_publish_topic_empty() {
        assert_eq!(validate_publish_topic(""), Err(TopicValidationError::Empty));
    }

    #[test]
    fn test_publish_topic_null_char() {
        assert_eq!(
            validate_publish_topic("foo\0bar"),
            Err(TopicValidationError::ContainsNullChar)
        );
    }

    #[test]
    fn test_publish_topic_wildcards_rejected() {
        assert_eq!(
            validate_publish_topic("sensors/+/room1"),
            Err(TopicValidationError::WildcardInPublishTopic)
        );
        assert_eq!(
            validate_publish_topic("sensors/#"),
            Err(TopicValidationError::WildcardInPublishTopic)
        );
        assert_eq!(
            validate_publish_topic("+"),
            Err(TopicValidationError::WildcardInPublishTopic)
        );
        assert_eq!(
            validate_publish_topic("#"),
            Err(TopicValidationError::WildcardInPublishTopic)
        );
    }

    #[test]
    fn test_publish_topic_too_long() {
        let long_topic = "a".repeat(MAX_TOPIC_LENGTH + 1);
        assert_eq!(
            validate_publish_topic(&long_topic),
            Err(TopicValidationError::TooLong)
        );

        // Exactly at limit should be fine
        let max_topic = "a".repeat(MAX_TOPIC_LENGTH);
        assert!(validate_publish_topic(&max_topic).is_ok());
    }

    #[test]
    fn test_valid_subscribe_filters() {
        // Exact topics (no wildcards)
        assert!(validate_subscribe_filter("a").is_ok());
        assert!(validate_subscribe_filter("a/b/c").is_ok());
        assert!(validate_subscribe_filter("sensors/temperature/room1").is_ok());

        // Single-level wildcard
        assert!(validate_subscribe_filter("+").is_ok());
        assert!(validate_subscribe_filter("+/+/+").is_ok());
        assert!(validate_subscribe_filter("sensors/+/room1").is_ok());
        assert!(validate_subscribe_filter("+/temperature/+").is_ok());

        // Multi-level wildcard
        assert!(validate_subscribe_filter("#").is_ok());
        assert!(validate_subscribe_filter("sensors/#").is_ok());
        assert!(validate_subscribe_filter("sensors/temperature/#").is_ok());

        // Combined wildcards
        assert!(validate_subscribe_filter("+/#").is_ok());
        assert!(validate_subscribe_filter("sensors/+/#").is_ok());
        assert!(validate_subscribe_filter("+/+/#").is_ok());

        // Edge cases
        assert!(validate_subscribe_filter("/").is_ok());
        assert!(validate_subscribe_filter("/+").is_ok());
        assert!(validate_subscribe_filter("/#").is_ok());
    }

    #[test]
    fn test_subscribe_filter_empty() {
        assert_eq!(
            validate_subscribe_filter(""),
            Err(TopicValidationError::Empty)
        );
    }

    #[test]
    fn test_subscribe_filter_null_char() {
        assert_eq!(
            validate_subscribe_filter("foo\0bar"),
            Err(TopicValidationError::ContainsNullChar)
        );
    }

    #[test]
    fn test_subscribe_filter_invalid_single_wildcard() {
        // + must occupy entire level
        assert_eq!(
            validate_subscribe_filter("sensors/temp+/room1"),
            Err(TopicValidationError::InvalidSingleLevelWildcard)
        );
        assert_eq!(
            validate_subscribe_filter("sensors/+temp/room1"),
            Err(TopicValidationError::InvalidSingleLevelWildcard)
        );
        assert_eq!(
            validate_subscribe_filter("sensors/te+mp/room1"),
            Err(TopicValidationError::InvalidSingleLevelWildcard)
        );
        assert_eq!(
            validate_subscribe_filter("+abc"),
            Err(TopicValidationError::InvalidSingleLevelWildcard)
        );
    }

    #[test]
    fn test_subscribe_filter_invalid_multi_wildcard() {
        // # must be at end
        assert_eq!(
            validate_subscribe_filter("sensors/#/room1"),
            Err(TopicValidationError::InvalidMultiLevelWildcard)
        );
        assert_eq!(
            validate_subscribe_filter("#/sensors"),
            Err(TopicValidationError::InvalidMultiLevelWildcard)
        );

        // # must occupy entire level
        assert_eq!(
            validate_subscribe_filter("sensors/temp#"),
            Err(TopicValidationError::InvalidMultiLevelWildcard)
        );
        assert_eq!(
            validate_subscribe_filter("sensors/#temp"),
            Err(TopicValidationError::InvalidMultiLevelWildcard)
        );
        assert_eq!(
            validate_subscribe_filter("#abc"),
            Err(TopicValidationError::InvalidMultiLevelWildcard)
        );
    }

    #[test]
    fn test_subscribe_filter_too_long() {
        let long_filter = "a".repeat(MAX_TOPIC_LENGTH + 1);
        assert_eq!(
            validate_subscribe_filter(&long_filter),
            Err(TopicValidationError::TooLong)
        );
    }
}
