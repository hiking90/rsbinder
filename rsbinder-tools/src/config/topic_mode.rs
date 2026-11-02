// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What a hub warns of about a configuration, by how it serves topics.
//!
//! A hub on kernel binder finds its registry through the service manager, so `[[topic]]` rules
//! alone leave it unreachable. A hub on a Unix socket (`rsb_hub --topic-socket`) has no service
//! manager at all, so what governs services is left with nothing to govern. Neither is refused:
//! one directory of configuration can serve a hub of each kind.

use rsbinder_topic::aidl::ITopicRegistry::SERVICE_NAME;

use super::policy::Subjects;
use super::Config;

/// A hub on kernel binder: why the registry the `[[topic]]` rules govern cannot be found, if so.
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

/// A hub on a Unix socket: what the configuration sets that it does not use, if anything.
pub fn unused_on_a_topic_socket(config: &Config) -> Option<String> {
    let rules = config.policy.rules.len();
    let services = config.declarations.len();
    let list = config.policy.list != Subjects::None;
    if rules == 0 && services == 0 && !list {
        return None;
    }
    let mut unused = Vec::new();
    if rules > 0 {
        unused.push(format!("{rules} [[rule]]"));
    }
    if services > 0 {
        unused.push(format!(
            "{services} [[service]], whose `start` is not run: this mode starts no service"
        ));
    }
    if list {
        unused.push("[global] list".to_owned());
    }
    Some(format!(
        "this hub serves topics on a socket and no service manager, so it does not use: {}",
        unused.join(", ")
    ))
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
    fn no_topic_rule_warns_of_nothing_on_kernel_binder() {
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

    #[test]
    fn topic_rules_alone_are_all_a_socket_hub_uses() {
        assert_eq!(unused_on_a_topic_socket(&with_topic_rule()), None);
    }

    #[test]
    fn what_a_socket_hub_does_not_use_is_named() {
        let mut config = with_topic_rule();
        config.policy.rules = vec![rule("*", Subjects::Any), rule("a", Subjects::Any)];
        config.policy.list = Subjects::Any;
        let warning = unused_on_a_topic_socket(&config).unwrap();
        assert!(warning.contains("2 [[rule]]"), "{warning}");
        assert!(warning.contains("[global] list"), "{warning}");
        assert!(!warning.contains("[[service]]"), "{warning}");
    }

    /// A kernel hub's service manager rules are no reason to warn a socket hub of the registry.
    #[test]
    fn the_two_warnings_are_of_the_two_modes() {
        let config = with_topic_rule();
        assert!(registry_out_of_reach(&config).is_some());
        assert_eq!(unused_on_a_topic_socket(&config), None);
    }
}
