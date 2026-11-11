//! Native path selection tied to the originating form and input entity.

use super::{Form, WorkspaceView};
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, button::ButtonVariants as _};
use gpui_kit::{AnyElement, Context, IntoElement, Window, div, prelude::*, px};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) enum PathKind {
    File,
    Directory,
}

impl Form {
    pub(super) fn path_field(
        &self,
        key: &'static str,
        label: &str,
        kind: PathKind,
        cx: &mut Context<WorkspaceView>,
    ) -> AnyElement {
        let id = self.id;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_size(px(12.)).child(label.to_string()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .children(
                        self.fields
                            .get(key)
                            .map(|input| crate::controls::input(input, cx).flex_1().min_w_0()),
                    )
                    .child(
                        crate::controls::button(format!("browse-{id}-{key}"))
                            .ghost()
                            .compact()
                            .icon(Icon::new(IconName::FolderOpen).size_4())
                            .label(crate::i18n::text("浏览…"))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.browse_form_path(id, key, kind, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl WorkspaceView {
    fn path_form_mut(&mut self, id: Uuid) -> Option<&mut Form> {
        if let Some(form) = &mut self.form
            && form.id == id
        {
            return Some(form);
        }
        self.integration_editor
            .as_mut()
            .map(|editor| &mut editor.form)
            .filter(|form| form.id == id)
    }

    pub(super) fn browse_form_path(
        &mut self,
        id: Uuid,
        key: &'static str,
        kind: PathKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        let Some(input) = self
            .path_form_mut(id)
            .and_then(|form| form.fields.get(key))
            .map(|input| input.downgrade())
        else {
            return;
        };
        let prompt = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: matches!(kind, PathKind::File),
            directories: matches!(kind, PathKind::Directory),
            multiple: false,
            prompt: Some(
                crate::i18n::text(match kind {
                    PathKind::File => "选择文件",
                    PathKind::Directory => "选择文件夹",
                })
                .into(),
            ),
        });
        cx.spawn_in(window, async move |view, cx| {
            let selection = prompt.await;
            let _ = view.update_in(cx, |view, window, cx| {
                if view.configuration_save.is_some() || view.data_busy.is_some() {
                    return;
                }
                let Some(form) = view.path_form_mut(id) else {
                    return;
                };
                // A reopened editor may reuse a connection UUID, but never its input entity.
                if !form
                    .fields
                    .get(key)
                    .is_some_and(|field| field.downgrade() == input)
                {
                    return;
                }
                match selection {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            if key == "shell" {
                                if !crate::shells::executable(&path) {
                                    view.status =
                                        Some(crate::i18n::text("请选择可执行的 Shell 程序").into());
                                    cx.notify();
                                    return;
                                }
                                form.shell =
                                    Some(super::settings::ShellEditor::new(&path, window, cx));
                            }
                            form.connection_probe = None;
                            form.probe_result = None;
                            let field = form.fields[key].clone();
                            field.update(cx, |state, cx| {
                                state.set_value(path.to_string_lossy().into_owned(), window, cx);
                                state.focus(window, cx);
                            });
                        }
                    }
                    Ok(Err(error)) => view.status = Some(error.to_string()),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }
}
