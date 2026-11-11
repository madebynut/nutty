//! Modal SSH challenges are queued and never replace another connection's active prompt.

use super::*;

impl WorkspaceView {
    pub(super) fn advance_authentication(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_authentication
            .retain(|prompt| !prompt.answer.is_closed());
        if self
            .auth_dialog
            .as_ref()
            .is_some_and(|dialog| !dialog.prompt.answer.is_closed())
        {
            return;
        }
        self.auth_dialog = None;
        let Some(prompt) = self.pending_authentication.pop_front() else {
            return;
        };
        let fields: Vec<_> = prompt
            .prompts
            .iter()
            .map(|prompt| cx.new(|cx| InputState::new(window, cx).masked(!prompt.echo)))
            .collect();
        let subscriptions = fields
            .iter()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |view, input, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::PressEnter { .. })
                            && view.auth_dialog.as_ref().is_some_and(|dialog| {
                                dialog.fields.iter().any(|field| field == input)
                            })
                        {
                            view.answer_authentication(window, cx);
                        }
                    },
                )
            })
            .collect();
        if let Some(first) = fields.first() {
            first.update(cx, |input, cx| input.focus(window, cx));
        } else {
            self.modal_focus.focus(window, cx);
        }
        self.auth_dialog = Some(AuthDialog {
            prompt,
            fields,
            _subscriptions: subscriptions,
        });
    }

    pub(super) fn answer_authentication(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.auth_dialog.take() {
            let answers = dialog
                .fields
                .iter()
                .map(|input| input.read(cx).value().to_string())
                .collect();
            let _ = dialog.prompt.answer.send(Some(answers));
        }
        self.finish_authentication(window, cx);
    }

    pub(super) fn cancel_authentication(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.auth_dialog.take() {
            let _ = dialog.prompt.answer.send(None);
        }
        self.finish_authentication(window, cx);
    }

    fn finish_authentication(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.advance_authentication(window, cx);
        if self.auth_dialog.is_none() {
            if self.modal.is_some()
                || !self.prompts.is_empty()
                || self.file_editor.is_some()
                || self.file_action.is_some()
            {
                self.modal_focus.focus(window, cx);
            } else {
                self.focus_terminal(window, cx);
            }
        }
        cx.notify();
    }
}
