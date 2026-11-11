//! Searchable saved connections organized by their configured groups.

use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    Icon,
    menu::{DropdownMenu as _, PopupMenuItem},
};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, Transformation, div,
    percentage, prelude::*, px, rgb, rgba,
};

use super::{WorkspaceView, icon_button};
use crate::config::Connection;

struct ConnectionGroup<'a> {
    name: Option<&'a str>,
    connections: Vec<&'a Connection>,
}

fn connection_groups<'a>(connections: &'a [Connection], query: &str) -> Vec<ConnectionGroup<'a>> {
    let query = query.trim().to_lowercase();
    let mut groups: BTreeMap<Option<&str>, Vec<&Connection>> = BTreeMap::new();
    for connection in connections {
        if !format!(
            "{} {} {}",
            connection.name, connection.host, connection.group
        )
        .to_lowercase()
        .contains(&query)
        {
            continue;
        }
        let name = connection.group.trim();
        groups
            .entry((!name.is_empty()).then_some(name))
            .or_default()
            .push(connection);
    }
    // Keep ungrouped connections distinct from a user-created group named “未分组”.
    let ungrouped = groups.remove(&None);
    groups
        .into_iter()
        .map(|(name, connections)| ConnectionGroup { name, connections })
        .chain(ungrouped.map(|connections| ConnectionGroup {
            name: None,
            connections,
        }))
        .collect()
}

impl WorkspaceView {
    pub(super) fn render_connection_groups(
        &self,
        query: &str,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let groups = connection_groups(&self.store.profile.connections, query);
        // A successful pane owns this status until it is closed, including hidden tabs.
        // Registry entries, probes and failed opens must never turn a saved row green.
        let connected: HashSet<_> = self
            .panes
            .values()
            .filter_map(|pane| pane.node.as_ref().map(|node| node.connection.id))
            .collect();
        let p = self.palette();
        let searching = !query.trim().is_empty();
        if groups.is_empty() {
            return vec![
                div()
                    .px_3()
                    .py_3()
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(crate::i18n::text(
                        if self.store.profile.connections.is_empty() {
                            "还没有保存的连接"
                        } else {
                            "没有匹配的连接"
                        },
                    ))
                    .into_any_element(),
            ];
        }
        groups
            .into_iter()
            .map(|group| {
                let key = group.name.map(str::to_owned);
                let expanded = searching || !self.collapsed_connection_groups.contains(&key);
                let label = group.name.unwrap_or_else(|| crate::i18n::text("未分组"));
                let group_id = format!("connection-group-{key:?}");
                let heading = div()
                    .id(group_id)
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .px_2()
                    .py_2()
                    .mt_1()
                    .rounded_md()
                    .when(!searching, |heading| {
                        heading
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(p.hover)))
                    })
                    .text_size(px(
                        (self.store.profile.preferences.ui_font_size - 2.).max(12.)
                    ))
                    .text_color(rgb(p.muted))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if searching {
                            return;
                        }
                        if !view.collapsed_connection_groups.remove(&key) {
                            view.collapsed_connection_groups.insert(key.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_4()
                        .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_ellipsis()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(label.to_owned()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .child(group.connections.len().to_string()),
                    );
                div()
                    .child(heading)
                    .children(group.connections.into_iter().filter(|_| expanded).map(
                        |connection| {
                            self.render_connection_row(
                                connection,
                                connected.contains(&connection.id),
                                cx,
                            )
                        },
                    ))
                    .into_any_element()
            })
            .collect()
    }

    fn render_connection_row(
        &self,
        connection: &Connection,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette();

        let id = connection.id;
        let waiting = self.connecting.contains(&id);
        div()
            .id(format!("connection-{id}"))
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_3()
            .rounded_md()
            .border_1()
            .border_color(rgba(if waiting { (p.accent << 8) | 0x70 } else { 0 }))
            .when(waiting, |row| row.bg(rgba((p.accent << 8) | 0x18)))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(p.hover)))
            .on_click(cx.listener(move |view, _, _, cx| view.connect(id, None, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .flex_1()
                    .min_w_0()
                    .child(if waiting && !connected {
                        Icon::new(IconName::Loader)
                            .size_5()
                            .flex_shrink_0()
                            .text_color(rgb(p.accent))
                            .with_animation(
                                format!("connection-spinner-{id}"),
                                Animation::new(Duration::from_millis(1000))
                                    .repeat()
                                    .with_max_fps(30.),
                                |icon, progress| {
                                    icon.transform(Transformation::rotate(percentage(progress)))
                                },
                            )
                            .into_any_element()
                    } else {
                        Icon::new(IconName::Server)
                            .size_5()
                            .flex_shrink_0()
                            .text_color(rgb(if connected { p.success } else { p.muted }))
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(self
                                        .store
                                        .profile
                                        .preferences
                                        .ui_font_size
                                        .max(14.)))
                                    .text_ellipsis()
                                    .child(connection.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px((self.store.profile.preferences.ui_font_size
                                        - 2.)
                                        .max(12.)))
                                    .text_color(rgb(if waiting { p.accent } else { p.muted }))
                                    .when(waiting, |status| {
                                        status.font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    })
                                    .text_ellipsis()
                                    .child(if waiting {
                                        crate::i18n::text("连接中…").into()
                                    } else {
                                        format!("{}@{}", connection.user, connection.host)
                                    }),
                            ),
                    ),
            )
            .child(
                icon_button(
                    format!("edit-{id}"),
                    IconName::Ellipsis,
                    crate::i18n::text("连接操作"),
                )
                .disabled(self.configuration_save.is_some() || self.data_busy.is_some())
                .on_click(|_, _, cx| cx.stop_propagation())
                .dropdown_menu({
                    let workspace = cx.entity().downgrade();
                    move |menu, _, _| {
                        let editing = workspace.clone();
                        let removing = workspace.clone();
                        menu.item(
                            PopupMenuItem::new(crate::i18n::text("编辑连接"))
                                .icon(IconName::SquarePen)
                                .on_click(move |_, window, cx| {
                                    let _ = editing.update(cx, |view, cx| {
                                        view.connection_form(Some(id), window, cx)
                                    });
                                }),
                        )
                        .item(
                            PopupMenuItem::new(crate::i18n::text("删除连接"))
                                .icon(IconName::Trash)
                                .on_click(move |_, window, cx| {
                                    let _ = removing.update(cx, |view, cx| {
                                        view.request_connection_removal(id, cx);
                                        view.modal_focus.focus(window, cx);
                                    });
                                }),
                        )
                    }
                }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Authentication, ConnectionKind};
    use uuid::Uuid;

    fn connection(name: &str, host: &str, group: &str) -> Connection {
        Connection {
            id: Uuid::new_v4(),
            name: name.into(),
            host: host.into(),
            group: group.into(),
            port: 22,
            user: "tester".into(),
            kind: ConnectionKind::Ssh,
            initial_directory: ".".into(),
            auth: Authentication::Agent,
            ssh: Default::default(),
            jump: None,
            proxy: None,
        }
    }

    #[test]
    fn grouping_normalizes_spaces_preserves_order_and_distinguishes_ungrouped() {
        let connections = vec![
            connection("api", "one.test", " Work "),
            connection("other", "two.test", " "),
            connection("db", "three.test", "Work"),
            connection("named", "four.test", "未分组"),
        ];
        let groups = connection_groups(&connections, "");
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, Some("Work"));
        assert_eq!(
            groups[0]
                .connections
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![connections[0].id, connections[2].id]
        );
        assert_eq!(groups[1].name, Some("未分组"));
        assert_eq!(groups[2].name, None);
        assert_eq!(groups[2].connections[0].id, connections[1].id);
    }

    #[test]
    fn search_matches_names_hosts_and_whole_groups_without_empty_headings() {
        let connections = vec![
            connection("API", "app.test", "Production"),
            connection("Database", "db.test", "Production"),
            connection("Sandbox", "sandbox.test", "Lab"),
        ];
        let group = connection_groups(&connections, " PRODUCTION ");
        assert_eq!(group.len(), 1);
        assert_eq!(group[0].connections.len(), 2);
        let name = connection_groups(&connections, " api ");
        assert_eq!(name.len(), 1);
        assert_eq!(name[0].connections[0].id, connections[0].id);
        let host = connection_groups(&connections, "DB.TEST");
        assert_eq!(host.len(), 1);
        assert_eq!(host[0].connections[0].id, connections[1].id);
        assert!(connection_groups(&connections, "missing").is_empty());
    }

    #[test]
    fn saved_group_changes_move_the_same_connection_and_remove_empty_groups() {
        let mut connections = vec![connection("api", "app.test", "Old")];
        let id = connections[0].id;
        connections[0].group = "New".into();
        let groups = connection_groups(&connections, "");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, Some("New"));
        assert_eq!(groups[0].connections[0].id, id);
        connections[0].group.clear();
        let groups = connection_groups(&connections, "");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, None);
        assert_eq!(groups[0].connections[0].id, id);
    }
}
