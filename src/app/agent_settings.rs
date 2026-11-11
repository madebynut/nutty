//! Pane-owned Agent discovery and confirmed configuration changes.

use anyhow::Result;
use gpui_kit::{Context, Window};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{BackendEvent, WorkspaceView, model_picker::Target};
use crate::{acp::configuration::Configuration, integrations::Agent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    pub session: Uuid,
    pub agent: Uuid,
    pub generation: Uuid,
}

#[derive(Default)]
pub(super) struct AgentSettings {
    pub agent: Option<Uuid>,
    pub session: Uuid,
    pub configuration: Configuration,
    pub loading: bool,
    pub error: Option<String>,
    load: Option<(Uuid, CancellationToken)>,
    change: Option<(Uuid, CancellationToken)>,
}

impl AgentSettings {
    pub fn changing(&self, session: Uuid) -> bool {
        self.session == session && self.change.is_some()
    }
    fn accepts(&self, identity: Identity) -> bool {
        self.session == identity.session
            && self.agent == Some(identity.agent)
            && self.load.as_ref().is_some_and(|(generation, cancel)| {
                *generation == identity.generation && !cancel.is_cancelled()
            })
    }
    fn update(&mut self, configuration: Configuration) {
        // A request completion can arrive after a newer watch notification.
        if configuration.revision() >= self.configuration.revision() {
            self.configuration = configuration;
        }
    }
    fn cancel(&mut self) {
        for request in [&mut self.load, &mut self.change] {
            if let Some((_, cancel)) = request.take() {
                cancel.cancel();
            }
        }
    }
}

impl Drop for AgentSettings {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl WorkspaceView {
    pub(super) fn selected_agent(&self, pane: Uuid, cx: &gpui_kit::App) -> Option<Agent> {
        let Target::Agent(id) = self.assistant_target(pane, cx)? else {
            return None;
        };
        self.store
            .profile
            .agents
            .iter()
            .find(|agent| agent.id == id && agent.enabled)
            .cloned()
    }

    pub(super) fn load_agent_configuration(
        &mut self,
        id: Uuid,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(agent) = self.selected_agent(id, cx) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let session = pane.assistant.session.id;
        let state = &mut pane.assistant.agent_settings;
        if pane.assistant.session.ended
            || !force
                && state.agent == Some(agent.id)
                && state.session == session
                && state.load.is_some()
        {
            return;
        }
        state.cancel();
        *state = AgentSettings::default();
        state.agent = Some(agent.id);
        state.session = session;
        state.loading = true;
        let request = Uuid::new_v4();
        let identity = Identity {
            session,
            agent: agent.id,
            generation: request,
        };
        let cancel = CancellationToken::new();
        state.load = Some((request, cancel.clone()));
        let runtime = pane.assistant.session.runtime.clone();
        let (permissions, receive) = async_channel::bounded(8);
        let events = self.events.clone();
        let permission_cancel = runtime.cancel.clone();
        self.launch(async move {
            loop {
                let permission = tokio::select! { ()=permission_cancel.cancelled()=>break, result=receive.recv()=>{ let Ok(permission)=result else { break }; permission } };
                if events.send(BackendEvent::AgentPermission { pane:id, session, permission }).await.is_err() { break; }
            }
        });
        let events = self.events.clone();
        self.launch(async move {
            let peer = runtime.agent_peer(&agent, permissions, cancel.clone()).await;
            let peer = match peer {
                Ok(peer) => peer,
                Err(error) => {
                    let _ = events.send(BackendEvent::AgentConfiguration { pane:id, identity, request:Some(request), result:Err(error) }).await;
                    return;
                }
            };
            let mut configuration = peer.configuration();
            let result = Ok(configuration.borrow_and_update().clone());
            if events.send(BackendEvent::AgentConfiguration { pane:id, identity, request:Some(request), result }).await.is_err() { return; }
            loop {
                tokio::select! { ()=cancel.cancelled()=>break, ()=runtime.cancel.cancelled()=>break, result=configuration.changed()=>{ if result.is_err() { break; } } }
                let result = Ok(configuration.borrow_and_update().clone());
                if events.send(BackendEvent::AgentConfiguration { pane:id, identity, request:None, result }).await.is_err() { break; }
            }
        });
        cx.notify();
    }

    pub(super) fn configure_agent(
        &mut self,
        id: Uuid,
        config: String,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        if self.assistant_locked(id, cx) {
            return;
        }
        let Some(agent) = self.selected_agent(id, cx) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let session = pane.assistant.session.id;
        let state = &mut pane.assistant.agent_settings;
        if state.loading || state.agent != Some(agent.id) || state.session != session {
            return;
        }
        let Some((generation, load_cancel)) = &state.load else {
            return;
        };
        if load_cancel.is_cancelled() {
            return;
        }
        let identity = Identity {
            session,
            agent: agent.id,
            generation: *generation,
        };
        let Some(setting) = state
            .configuration
            .settings
            .iter()
            .find(|setting| setting.id == config)
        else {
            return;
        };
        if setting.current == value {
            return;
        }
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        state.change = Some((request, cancel.clone()));
        state.error = None;
        let runtime = pane.assistant.session.runtime.clone();
        let events = self.events.clone();
        self.launch(async move {
            // Configuration uses the existing peer so the conversation is retained.
            let result = runtime
                .configure_agent(agent.id, config, value, cancel)
                .await;
            let _ = events
                .send(BackendEvent::AgentConfiguration {
                    pane: id,
                    identity,
                    request: Some(request),
                    result,
                })
                .await;
        });
        cx.notify();
    }

    pub(super) fn receive_agent_configuration(
        &mut self,
        id: Uuid,
        identity: Identity,
        request: Option<Uuid>,
        result: Result<Configuration>,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let state = &mut pane.assistant.agent_settings;
        if pane.assistant.session.ended
            || pane.assistant.session.id != identity.session
            || !state.accepts(identity)
        {
            return;
        }
        if let Some(request) = request {
            if state
                .change
                .as_ref()
                .is_some_and(|(id, cancel)| *id == request && !cancel.is_cancelled())
            {
                state.change = None;
            } else if state
                .load
                .as_ref()
                .is_some_and(|(id, cancel)| *id == request && !cancel.is_cancelled())
            {
                state.loading = false;
            } else {
                return;
            }
        }
        match result {
            Ok(configuration) => {
                state.update(configuration);
                state.error = None;
                if request.is_some() {
                    pane.assistant.session.error = None;
                    pane.assistant.session.needs_agent_login = false;
                }
            }
            Err(error) => {
                pane.assistant.session.needs_agent_login =
                    error.downcast_ref::<crate::acp::LoginRequired>().is_some();
                state.error = Some(format!("{error:#}"));
                pane.assistant.session.error = state.error.clone();
            }
        }
        cx.notify();
    }

    pub(super) fn choose_agent_model(
        &mut self,
        id: Uuid,
        agent: Uuid,
        config: String,
        value: String,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .selected_agent(id, cx)
            .is_some_and(|selected| selected.id == agent)
        {
            self.configure_agent(id, config, value.into(), cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_request_completion_does_not_overwrite_newer_watch_configuration() {
        let mut configuration = Configuration::from_response(
            &serde_json::json!({"models":{"currentModelId":"other","availableModels":[{"modelId":"other","name":"Other"},{"modelId":"auto","name":"Auto"},{"modelId":"grok-4.7","name":"Grok 4.7"}]}}),
        );
        let id = configuration.model().expect("initial model").id.clone();
        configuration.set_current(&id, "auto".into());
        let completion = configuration.clone();
        configuration.set_current(&id, "grok-4.7".into());
        let mut state = AgentSettings::default();
        state.update(configuration);
        state.update(completion);
        assert_eq!(
            state
                .configuration
                .model()
                .expect("latest model")
                .current_label(),
            "Grok 4.7",
            "late completion from the same generation cannot undo a newer notification"
        );
        let revision = state.configuration.revision();
        let repeated =
            serde_json::json!({"sessionUpdate":"current_model_update","currentModelId":"grok-4.7"});
        assert!(!state.configuration.update_notification(&repeated));
        assert_eq!(state.configuration.revision(), revision);
    }

    #[test]
    fn refresh_reset_switch_and_cancel_reject_late_configuration_snapshots() {
        let first = Identity {
            session: Uuid::new_v4(),
            agent: Uuid::new_v4(),
            generation: Uuid::new_v4(),
        };
        let first_cancel = CancellationToken::new();
        let mut state = AgentSettings {
            session: first.session,
            agent: Some(first.agent),
            load: Some((first.generation, first_cancel.clone())),
            configuration: Configuration::default(),
            loading: false,
            error: None,
            change: None,
        };
        assert!(state.accepts(first));
        state.cancel();
        let refreshed = Identity {
            generation: Uuid::new_v4(),
            ..first
        };
        state.load = Some((refreshed.generation, CancellationToken::new()));
        assert!(
            !state.accepts(first),
            "old watch must not overwrite a refresh for the same Agent/session"
        );
        assert!(first_cancel.is_cancelled());
        assert!(state.accepts(refreshed));
        let reset = Identity {
            session: Uuid::new_v4(),
            ..refreshed
        };
        state.session = reset.session;
        assert!(
            !state.accepts(refreshed),
            "previous AI conversation cannot update a reset session"
        );
        assert!(state.accepts(reset));
        let switched = Identity {
            agent: Uuid::new_v4(),
            ..reset
        };
        state.agent = Some(switched.agent);
        assert!(
            !state.accepts(reset),
            "previous Agent cannot update the selected Agent"
        );
        assert!(state.accepts(switched));
        state.load.as_ref().expect("active watch").1.cancel();
        assert!(
            !state.accepts(switched),
            "cancelled watch snapshots are rejected even before its task stops"
        );
        state.cancel();
        assert!(!state.accepts(switched));
    }
}
