// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::layout::{Constraint, Layout, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerBodyLayout {
    TableOnly { table: Rect },
    DetailsOnly { details: Rect },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerScreenLayout {
    pub search: Option<Rect>,
    pub summary: Rect,
    pub body: PeerBodyLayout,
    pub footer: Rect,
}

pub fn calculate_peer_screen_layout(
    area: Rect,
    search_visible: bool,
    show_details: bool,
) -> PeerScreenLayout {
    let content = peer_content_area(area);
    let search_height = u16::from(search_visible).saturating_mul(3);
    let vertical = Layout::vertical([
        Constraint::Length(search_height),
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .split(content);

    let search = search_visible.then_some(vertical[0]);
    let summary = vertical[1];
    let body_area = vertical[2];
    let footer = vertical[3];

    let body = if show_details {
        PeerBodyLayout::DetailsOnly { details: body_area }
    } else {
        PeerBodyLayout::TableOnly { table: body_area }
    };

    PeerScreenLayout {
        search,
        summary,
        body,
        footer,
    }
}

fn peer_content_area(area: Rect) -> Rect {
    if area.width < 90 || area.height < 18 {
        return area;
    }

    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_replace_the_table_at_every_screen_width() {
        let table = calculate_peer_screen_layout(Rect::new(0, 0, 150, 40), false, false);
        assert!(matches!(table.body, PeerBodyLayout::TableOnly { .. }));

        let details = calculate_peer_screen_layout(Rect::new(0, 0, 150, 40), false, true);
        assert!(matches!(details.body, PeerBodyLayout::DetailsOnly { .. }));

        let narrow_details = calculate_peer_screen_layout(Rect::new(0, 0, 70, 24), false, true);
        assert!(matches!(
            narrow_details.body,
            PeerBodyLayout::DetailsOnly { .. }
        ));
    }

    #[test]
    fn search_reserves_a_standard_prompt_row() {
        let without_search = calculate_peer_screen_layout(Rect::new(0, 0, 100, 30), false, false);
        let with_search = calculate_peer_screen_layout(Rect::new(0, 0, 100, 30), true, false);

        assert!(with_search.search.is_some());
        assert_eq!(with_search.search.unwrap().height, 3);
        let PeerBodyLayout::TableOnly {
            table: without_table,
        } = without_search.body
        else {
            panic!("expected table layout");
        };
        let PeerBodyLayout::TableOnly { table: with_table } = with_search.body else {
            panic!("expected table layout");
        };
        let without_table_height = without_table.height;
        let with_table_height = with_table.height;
        assert!(with_table_height < without_table_height);
    }
}
