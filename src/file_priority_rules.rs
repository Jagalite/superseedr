// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crate::app::FilePriority;
use crate::config::{FilePriorityRule, FilePriorityRuleTarget};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleMatch {
    pub rule_index: usize,
    pub rule_name: String,
    pub priority: FilePriority,
    pub winning: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleEvaluation {
    pub priorities: HashMap<usize, FilePriority>,
    pub matches: HashMap<usize, Vec<RuleMatch>>,
}

pub fn validate_rule(rule: &FilePriorityRule) -> Result<(), String> {
    if rule.name.trim().is_empty() {
        return Err("Rule name cannot be empty".to_string());
    }
    if rule.pattern.trim().is_empty() {
        return Err("Rule pattern cannot be empty".to_string());
    }
    if matches!(rule.priority, FilePriority::Mixed) {
        return Err("Mixed is a display-only priority".to_string());
    }
    if matches!(rule.target, FilePriorityRuleTarget::Extension)
        && rule.pattern.contains(['/', '\\'])
    {
        return Err("Extension patterns cannot contain path separators".to_string());
    }
    Ok(())
}

pub fn evaluate_file_priority_rules(
    rules: &[FilePriorityRule],
    file_paths: &[Vec<String>],
) -> RuleEvaluation {
    let mut result = RuleEvaluation::default();

    for (file_index, parts) in file_paths.iter().enumerate() {
        let name = parts.last().map(String::as_str).unwrap_or_default();
        let path = parts.join("/");
        let mut file_matches = Vec::new();
        let mut winner_selected = false;

        for (rule_index, rule) in rules.iter().enumerate() {
            if !rule.enabled || validate_rule(rule).is_err() || !rule_matches(rule, name, &path) {
                continue;
            }

            let winning = !winner_selected;
            if winning {
                winner_selected = true;
                if !matches!(rule.priority, FilePriority::Normal) {
                    result.priorities.insert(file_index, rule.priority);
                }
            }
            file_matches.push(RuleMatch {
                rule_index,
                rule_name: rule.name.clone(),
                priority: rule.priority,
                winning,
            });
        }

        if !file_matches.is_empty() {
            result.matches.insert(file_index, file_matches);
        }
    }

    result
}

fn rule_matches(rule: &FilePriorityRule, name: &str, path: &str) -> bool {
    match rule.target {
        FilePriorityRuleTarget::Extension => extension_matches(name, &rule.pattern),
        FilePriorityRuleTarget::Name => wildcard_match(&rule.pattern, name),
        FilePriorityRuleTarget::Path => wildcard_match(&rule.pattern, path),
    }
}

fn extension_matches(name: &str, pattern: &str) -> bool {
    let normalized = pattern
        .trim()
        .trim_start_matches('*')
        .trim_start_matches('.')
        .to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    name.to_lowercase().ends_with(&format!(".{normalized}"))
}

fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    let pattern: Vec<char> = pattern.trim().to_lowercase().chars().collect();
    let candidate: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut previous = vec![false; candidate.len() + 1];
    previous[0] = true;

    for token in pattern {
        let mut current = vec![false; candidate.len() + 1];
        if token == '*' {
            current[0] = previous[0];
        }
        for column in 1..=candidate.len() {
            current[column] = match token {
                '*' => previous[column] || current[column - 1],
                '?' => previous[column - 1],
                literal => previous[column - 1] && literal == candidate[column - 1],
            };
        }
        previous = current;
    }

    previous[candidate.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(
        name: &str,
        target: FilePriorityRuleTarget,
        pattern: &str,
        priority: FilePriority,
    ) -> FilePriorityRule {
        FilePriorityRule {
            name: name.to_string(),
            target,
            pattern: pattern.to_string(),
            priority,
            ..FilePriorityRule::default()
        }
    }

    #[test]
    fn extension_rules_are_case_insensitive_and_accept_a_leading_dot() {
        let rules = vec![rule(
            "Skip executables",
            FilePriorityRuleTarget::Extension,
            ".exe",
            FilePriority::Skip,
        )];
        let paths = vec![vec!["tools".into(), "helper.EXE".into()]];

        let evaluation = evaluate_file_priority_rules(&rules, &paths);

        assert_eq!(evaluation.priorities.get(&0), Some(&FilePriority::Skip));
        assert!(evaluation.matches[&0][0].winning);
    }

    #[test]
    fn name_and_path_rules_support_glob_wildcards() {
        let rules = vec![
            rule(
                "Prefer samples",
                FilePriorityRuleTarget::Name,
                "sample-??.bin",
                FilePriority::High,
            ),
            rule(
                "Deprioritize extras",
                FilePriorityRuleTarget::Path,
                "bundle/extras/*",
                FilePriority::Low,
            ),
        ];
        let paths = vec![
            vec!["bundle".into(), "sample-01.bin".into()],
            vec!["bundle".into(), "extras".into(), "notes.txt".into()],
        ];

        let evaluation = evaluate_file_priority_rules(&rules, &paths);

        assert_eq!(evaluation.priorities.get(&0), Some(&FilePriority::High));
        assert_eq!(evaluation.priorities.get(&1), Some(&FilePriority::Low));
    }

    #[test]
    fn first_enabled_match_wins_but_all_matches_are_reported() {
        let mut disabled = rule(
            "Disabled",
            FilePriorityRuleTarget::Name,
            "*",
            FilePriority::High,
        );
        disabled.enabled = false;
        let rules = vec![
            disabled,
            rule(
                "Skip text",
                FilePriorityRuleTarget::Extension,
                "txt",
                FilePriority::Skip,
            ),
            rule(
                "Everything normal",
                FilePriorityRuleTarget::Name,
                "*",
                FilePriority::Normal,
            ),
        ];

        let evaluation =
            evaluate_file_priority_rules(&rules, &[vec!["fictional-notes.txt".into()]]);

        assert_eq!(evaluation.priorities.get(&0), Some(&FilePriority::Skip));
        assert_eq!(evaluation.matches[&0].len(), 2);
        assert!(evaluation.matches[&0][0].winning);
        assert!(!evaluation.matches[&0][1].winning);
    }

    #[test]
    fn invalid_and_disabled_rules_do_not_match() {
        let rules = vec![FilePriorityRule::default()];
        let evaluation = evaluate_file_priority_rules(&rules, &[vec!["sample.bin".into()]]);
        assert!(evaluation.priorities.is_empty());
        assert!(evaluation.matches.is_empty());
    }

    #[test]
    fn settings_toml_round_trip_preserves_ordered_rules() {
        let settings = crate::config::Settings {
            file_priority_rules: vec![
                rule(
                    "Skip packages",
                    FilePriorityRuleTarget::Extension,
                    ".pkg",
                    FilePriority::Skip,
                ),
                rule(
                    "Prefer primary files",
                    FilePriorityRuleTarget::Path,
                    "fictional-set/primary/*",
                    FilePriority::High,
                ),
            ],
            ..Default::default()
        };

        let encoded = toml::to_string(&settings).expect("serialize settings");
        let decoded: crate::config::Settings =
            toml::from_str(&encoded).expect("deserialize settings");

        assert_eq!(decoded.file_priority_rules, settings.file_priority_rules);
    }
}
