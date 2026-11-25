// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What a hub warns of about a configuration's topic rules.
//!
//! A caller finds the hub's registry through the service manager, so `[[topic]]` rules alone
//! leave it unreachable.

use rsbinder_topic::aidl::ITopicRegistry::SERVICE_NAME;

use super::policy::Subjects;
use super::Config;

/// Why the registry the `[[topic]]` rules govern cannot be found, if so.
pub fn registry_out_of_reach(config: &Config) -> Option<String> {
    if config.topics.rules.is_empty() {
        return None;
    }
    let rules = &config.policy.rules;
    let rule = rules.iter().find(|rule| rule.pattern.matches(SERVICE_NAME));
    let Some(rule) = rule else {
        return Some(format!(
            "there are [[topic]] rules, but no [[rule]] covers '{SERVICE_NAME}', so no caller \
             can find the registry and RegistryResolver::find answers NameNotFound"
        ));
    };
    (rule.find == Subjects::None).then(|| {
        format!(
            "there are [[topic]] rules, but the [[rule]] that covers '{SERVICE_NAME}' lets no \
             caller find it, so RegistryResolver::find answers NameNotFound"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NamePattern, Rule, TopicRule};

    fn with_topic_rule() -> Config {
        let mut config = Config::default();
        config.topics.rules = vec![TopicRule {
            pattern: NamePattern::Any,
            advertise: Subjects::Any,
            subscribe: Subjects::Any,
        }];
        config
    }

    fn rule(pattern: &str, find: Subjects) -> Rule {
        Rule {
            pattern: NamePattern::parse(pattern).unwrap(),
            add: Subjects::None,
            find,
        }
    }

    #[test]
    fn no_topic_rule_warns_of_nothing() {
        assert_eq!(registry_out_of_reach(&Config::default()), None);
    }

    #[test]
    fn a_topic_rule_with_no_rule_for_the_registry_is_out_of_reach() {
        let warning = registry_out_of_reach(&with_topic_rule()).unwrap();
        assert!(warning.contains("no [[rule]]"), "{warning}");
    }

    #[test]
    fn a_rule_that_lets_nobody_find_the_registry_is_out_of_reach() {
        let mut config = with_topic_rule();
        config.policy.rules = vec![rule(SERVICE_NAME, Subjects::None)];
        let warning = registry_out_of_reach(&config).unwrap();
        assert!(warning.contains("lets no"), "{warning}");
    }

    #[test]
    fn a_rule_that_lets_someone_find_it_is_no_warning() {
        let mut config = with_topic_rule();
        config.policy.rules = vec![rule(SERVICE_NAME, Subjects::set([1000], []))];
        assert_eq!(registry_out_of_reach(&config), None);
    }

    #[test]
    fn a_wildcard_rule_covers_the_registry() {
        let mut config = with_topic_rule();
        config.policy.rules = vec![rule("rsbinder.*", Subjects::Any)];
        assert_eq!(registry_out_of_reach(&config), None);
        config.policy.rules = vec![rule("*", Subjects::None)];
        assert!(registry_out_of_reach(&config).is_some());
    }

    #[test]
    fn the_first_rule_that_covers_the_registry_governs_it() {
        let mut config = with_topic_rule();
        config.policy.rules = vec![
            rule("rsbinder.*", Subjects::None),
            rule(SERVICE_NAME, Subjects::Any),
        ];
        assert!(registry_out_of_reach(&config).is_some());
    }
}
