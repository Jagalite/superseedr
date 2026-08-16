// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{App, AppMode, FilePriority, FilePriorityRuleEditField, FilePriorityRulesUiState};
use crate::config::{FilePriorityRule, FilePriorityRuleTarget};
use crate::file_priority_rules::validate_rule;
use crate::tui::screen_context::ScreenContext;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{prelude::*, widgets::*};

fn next_priority(priority: FilePriority) -> FilePriority {
    match priority {
        FilePriority::Normal | FilePriority::Mixed => FilePriority::Low,
        FilePriority::Low => FilePriority::Skip,
        FilePriority::Skip => FilePriority::High,
        FilePriority::High => FilePriority::Normal,
    }
}

fn priority_label(priority: FilePriority) -> &'static str {
    match priority {
        FilePriority::Normal => "Normal",
        FilePriority::Low => "Low",
        FilePriority::High => "High",
        FilePriority::Skip => "Skip",
        FilePriority::Mixed => "Mixed",
    }
}

fn field_label(field: FilePriorityRuleEditField) -> &'static str {
    match field {
        FilePriorityRuleEditField::Name => "Name",
        FilePriorityRuleEditField::Target => "Target",
        FilePriorityRuleEditField::Pattern => "Pattern",
        FilePriorityRuleEditField::Priority => "Priority",
        FilePriorityRuleEditField::Enabled => "Enabled",
    }
}

fn begin_add(state: &mut FilePriorityRulesUiState, rule_count: usize) {
    state.draft = Some(FilePriorityRule {
        name: format!("Rule {}", rule_count + 1),
        target: FilePriorityRuleTarget::Extension,
        pattern: String::new(),
        priority: FilePriority::Skip,
        enabled: true,
    });
    state.draft_index = None;
    state.edit_field = FilePriorityRuleEditField::Name;
    state.delete_confirm_armed = false;
    state.status_message = Some("Edit fields with Tab; Enter saves".to_string());
}

fn begin_edit(state: &mut FilePriorityRulesUiState, rules: &[FilePriorityRule], index: usize) {
    let Some(rule) = rules.get(index).cloned() else {
        return;
    };
    state.draft = Some(rule);
    state.draft_index = Some(index);
    state.edit_field = FilePriorityRuleEditField::Name;
    state.delete_confirm_armed = false;
    state.status_message = Some("Edit fields with Tab; Enter saves".to_string());
}

fn edit_text(draft: &mut FilePriorityRule, field: FilePriorityRuleEditField, text: &str) {
    let target = match field {
        FilePriorityRuleEditField::Name => Some(&mut draft.name),
        FilePriorityRuleEditField::Pattern => Some(&mut draft.pattern),
        _ => None,
    };
    if let Some(target) = target {
        target.extend(text.chars().filter(|ch| !ch.is_control()));
    }
}

fn backspace_text(draft: &mut FilePriorityRule, field: FilePriorityRuleEditField) {
    match field {
        FilePriorityRuleEditField::Name => {
            draft.name.pop();
        }
        FilePriorityRuleEditField::Pattern => {
            draft.pattern.pop();
        }
        _ => {}
    }
}

fn cycle_draft_value(draft: &mut FilePriorityRule, field: FilePriorityRuleEditField) {
    match field {
        FilePriorityRuleEditField::Target => draft.target = draft.target.next(),
        FilePriorityRuleEditField::Priority => draft.priority = next_priority(draft.priority),
        FilePriorityRuleEditField::Enabled => draft.enabled = !draft.enabled,
        _ => {}
    }
}

async fn save_rules(app: &mut App, rules: Vec<FilePriorityRule>) {
    let mut settings = app.client_configs.clone();
    settings.file_priority_rules = rules;
    app.apply_config_update_from_ui(settings).await;
}

async fn handle_edit_key(key: KeyEvent, app: &mut App) {
    let mut commit = None;
    {
        let state = &mut app.app_state.ui.file_priority_rules;
        let Some(draft) = state.draft.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                state.draft = None;
                state.draft_index = None;
                state.status_message = Some("Edit cancelled".to_string());
            }
            KeyCode::Tab | KeyCode::Down => state.edit_field = state.edit_field.next(),
            KeyCode::BackTab | KeyCode::Up => state.edit_field = state.edit_field.previous(),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                cycle_draft_value(draft, state.edit_field)
            }
            KeyCode::Backspace => backspace_text(draft, state.edit_field),
            KeyCode::Enter => match validate_rule(draft) {
                Ok(()) => commit = Some((state.draft_index, draft.clone())),
                Err(message) => state.status_message = Some(message),
            },
            KeyCode::Char(ch) => edit_text(draft, state.edit_field, &ch.to_string()),
            _ => {}
        }
    }

    if let Some((index, draft)) = commit {
        let mut rules = app.client_configs.file_priority_rules.clone();
        if let Some(index) = index {
            if let Some(rule) = rules.get_mut(index) {
                *rule = draft;
            }
        } else {
            rules.push(draft);
        }
        save_rules(app, rules).await;
        let state = &mut app.app_state.ui.file_priority_rules;
        state.draft = None;
        state.draft_index = None;
        state.selected_index = state.selected_index.min(
            app.client_configs
                .file_priority_rules
                .len()
                .saturating_sub(1),
        );
        state.status_message = Some("Rule saved".to_string());
    }
}

async fn handle_list_key(key: KeyEvent, app: &mut App) {
    let count = app.client_configs.file_priority_rules.len();
    let selected = app
        .app_state
        .ui
        .file_priority_rules
        .selected_index
        .min(count.saturating_sub(1));

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.app_state.mode = AppMode::Normal;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let state = &mut app.app_state.ui.file_priority_rules;
            state.selected_index = selected.saturating_sub(1);
            state.delete_confirm_armed = false;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let state = &mut app.app_state.ui.file_priority_rules;
            state.selected_index = (selected + 1).min(count.saturating_sub(1));
            state.delete_confirm_armed = false;
        }
        KeyCode::Char('a') => begin_add(&mut app.app_state.ui.file_priority_rules, count),
        KeyCode::Enter | KeyCode::Char('e') if count > 0 => begin_edit(
            &mut app.app_state.ui.file_priority_rules,
            &app.client_configs.file_priority_rules,
            selected,
        ),
        KeyCode::Char('x') | KeyCode::Char(' ') if count > 0 => {
            let mut rules = app.client_configs.file_priority_rules.clone();
            rules[selected].enabled = !rules[selected].enabled;
            save_rules(app, rules).await;
        }
        KeyCode::Char('p') if count > 0 => {
            let mut rules = app.client_configs.file_priority_rules.clone();
            rules[selected].priority = next_priority(rules[selected].priority);
            save_rules(app, rules).await;
        }
        KeyCode::Char('t') if count > 0 => {
            let mut rules = app.client_configs.file_priority_rules.clone();
            rules[selected].target = rules[selected].target.next();
            save_rules(app, rules).await;
        }
        KeyCode::Char('K') if selected > 0 => {
            let mut rules = app.client_configs.file_priority_rules.clone();
            rules.swap(selected, selected - 1);
            app.app_state.ui.file_priority_rules.selected_index = selected - 1;
            save_rules(app, rules).await;
        }
        KeyCode::Char('J') if selected + 1 < count => {
            let mut rules = app.client_configs.file_priority_rules.clone();
            rules.swap(selected, selected + 1);
            app.app_state.ui.file_priority_rules.selected_index = selected + 1;
            save_rules(app, rules).await;
        }
        KeyCode::Char('d') if count > 0 => {
            if app.app_state.ui.file_priority_rules.delete_confirm_armed {
                let mut rules = app.client_configs.file_priority_rules.clone();
                rules.remove(selected);
                save_rules(app, rules).await;
                let state = &mut app.app_state.ui.file_priority_rules;
                state.selected_index = selected.min(
                    app.client_configs
                        .file_priority_rules
                        .len()
                        .saturating_sub(1),
                );
                state.delete_confirm_armed = false;
                state.status_message = Some("Rule deleted".to_string());
            } else {
                let state = &mut app.app_state.ui.file_priority_rules;
                state.delete_confirm_armed = true;
                state.status_message = Some("Press d again to delete this rule".to_string());
            }
        }
        _ => {}
    }
}

pub async fn handle_event(event: CrosstermEvent, app: &mut App) {
    if let CrosstermEvent::Paste(text) = event {
        let state = &mut app.app_state.ui.file_priority_rules;
        if let Some(draft) = state.draft.as_mut() {
            edit_text(draft, state.edit_field, &text);
        }
        return;
    }

    let CrosstermEvent::Key(key) = event else {
        return;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }

    if app.app_state.ui.file_priority_rules.draft.is_some() {
        handle_edit_key(key, app).await;
    } else {
        handle_list_key(key, app).await;
    }
    app.app_state.ui.needs_redraw = true;
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let ctx = screen.theme;
    let state = &screen.ui.ui.file_priority_rules;
    let rules = &screen.settings.file_priority_rules;
    let area = f.area();
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.accent_sapphire())))
        .title(" Automatic File-Priority Rules ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let rows = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(inner);
    let columns =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(rows[0]);

    let selected = state.selected_index.min(rules.len().saturating_sub(1));
    let items = if rules.is_empty() {
        vec![ListItem::new("No rules. Press a to create one.")]
    } else {
        rules
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let enabled = if rule.enabled { "x" } else { " " };
                let text = format!(
                    "{:>2}. [{}] {} · {}:{} → {}",
                    index + 1,
                    enabled,
                    rule.name,
                    rule.target.label(),
                    rule.pattern,
                    priority_label(rule.priority)
                );
                let style = if index == selected {
                    Style::default()
                        .fg(ctx.state_selected())
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else if rule.enabled {
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text))
                } else {
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))
                };
                ListItem::new(text).style(style)
            })
            .collect()
    };
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::RIGHT).title(" Rules ")),
        columns[0],
    );

    let shown = state.draft.as_ref().or_else(|| rules.get(selected));
    let details = if let Some(rule) = shown {
        let active_field = |field| {
            if state.draft.is_some() && state.edit_field == field {
                "▶"
            } else {
                " "
            }
        };
        vec![
            Line::from(format!(
                "{} Name: {}",
                active_field(FilePriorityRuleEditField::Name),
                rule.name
            )),
            Line::from(format!(
                "{} Target: {}",
                active_field(FilePriorityRuleEditField::Target),
                rule.target.label()
            )),
            Line::from(format!(
                "{} Pattern: {}",
                active_field(FilePriorityRuleEditField::Pattern),
                rule.pattern
            )),
            Line::from(format!(
                "{} Priority: {}",
                active_field(FilePriorityRuleEditField::Priority),
                priority_label(rule.priority)
            )),
            Line::from(format!(
                "{} Enabled: {}",
                active_field(FilePriorityRuleEditField::Enabled),
                if rule.enabled { "yes" } else { "no" }
            )),
            Line::from(""),
            Line::from(if state.draft.is_some() {
                format!(
                    "Editing {} · Tab changes field",
                    field_label(state.edit_field)
                )
            } else {
                "Topmost enabled match wins".to_string()
            }),
        ]
    } else {
        vec![Line::from("Rules apply when new torrent metadata arrives.")]
    };
    f.render_widget(
        Paragraph::new(details)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" Details ")),
        columns[1],
    );

    let status = state.status_message.as_deref().unwrap_or(
        "Extension is exact; name/path support * and ? wildcards; matching ignores case",
    );
    f.render_widget(
        Paragraph::new(status).style(ctx.apply(Style::default().fg(ctx.state_info()))),
        rows[1],
    );
    let footer = if state.draft.is_some() {
        "Tab field  ←/→ change  Enter save  Esc cancel"
    } else {
        "a add  e edit  x toggle  K/J reorder  t target  p priority  d delete  q close"
    };
    f.render_widget(
        Paragraph::new(footer)
            .alignment(Alignment::Center)
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_priority_cycle_never_exposes_mixed() {
        let mut value = FilePriority::Normal;
        let mut seen = Vec::new();
        for _ in 0..4 {
            value = next_priority(value);
            seen.push(value);
        }
        assert_eq!(
            seen,
            vec![
                FilePriority::Low,
                FilePriority::Skip,
                FilePriority::High,
                FilePriority::Normal
            ]
        );
    }

    #[test]
    fn add_starts_with_fictional_editable_rule() {
        let mut state = FilePriorityRulesUiState::default();
        begin_add(&mut state, 2);
        let draft = state.draft.expect("draft");
        assert_eq!(draft.name, "Rule 3");
        assert_eq!(draft.priority, FilePriority::Skip);
    }
}
