//! Pane-owned AI conversations with cancellation, stale-response rejection and command state.

use crate::ai::{Reply, Turn};
#[cfg(test)]
use anyhow::Context as _;
use anyhow::{Result, ensure};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Shell,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandState {
    Suggested,
    Sent,
    Deferred(String),
}

#[derive(Clone, Debug)]
pub struct ProposedCommand {
    pub id: Uuid,
    pub text: String,
    pub language: Option<String>,
    pub state: CommandState,
}

impl ProposedCommand {
    pub fn matches_block(&self, language: Option<&str>, code: &str) -> bool {
        language.is_some_and(crate::command::shell_language)
            && self.language.as_deref().is_none_or(|expected| {
                language.is_some_and(|actual| expected.eq_ignore_ascii_case(actual))
            })
            && self.text == code.trim()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationState {
    Ready,
    ShellUnavailable,
    TerminalChanged,
}

impl Entry {
    pub fn prepare_automatic(
        &mut self,
        environment: &crate::session_environment::Environment,
        state: AutomationState,
    ) -> Vec<(Uuid, String)> {
        let mut dispatch = Vec::new();
        for proposed in &mut self.commands {
            if proposed.state == CommandState::Sent {
                continue;
            }
            let result = (|| -> Result<String> {
                ensure!(
                    proposed.language.as_deref().is_none_or(|language| {
                        crate::command::language_supported(language, environment)
                    }),
                    "命令语言与当前终端 Shell 不匹配，已阻止发送"
                );
                ensure!(
                    environment.supports_posix_commands()
                        || environment.supports_powershell_commands(),
                    "当前 Shell 暂不支持只读自动发送"
                );
                let command = if environment.supports_powershell_commands() {
                    crate::command::prepare_powershell_read_only(&proposed.text)?
                } else {
                    crate::command::prepare_read_only(&proposed.text)?
                };
                ensure!(
                    environment.supports_powershell_commands()
                        || !(environment.shell_is("fish") && command.contains("\"$0\"")),
                    "当前 Shell 不支持 $0，请手动调整命令"
                );
                match state {
                    AutomationState::Ready => {}
                    AutomationState::ShellUnavailable => {
                        anyhow::bail!("等待远端 Shell 集成：请在终端按 Enter 返回空提示符")
                    }
                    AutomationState::TerminalChanged => {
                        anyhow::bail!("终端状态已变化，或无法确认空闲提示符，请手动执行")
                    }
                }
                ensure!(
                    dispatch.len() < 3,
                    "每次最多自动发送 3 条命令，其余请手动执行"
                );
                Ok(command)
            })();
            match result {
                Ok(command) => dispatch.push((proposed.id, command)),
                Err(error) => {
                    proposed.state =
                        CommandState::Deferred(crate::i18n::localize(error.to_string()))
                }
            }
        }
        dispatch
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub turn: Turn,
    pub commands: Vec<ProposedCommand>,
    pub tools: Vec<crate::integrations::ProposedTool>,
}

pub struct Pending {
    pub id: Uuid,
    pub cancel: CancellationToken,
    pub terminal_revision: u64,
    pub partial: String,
}

pub struct Session {
    pub id: Uuid,
    pub entries: Vec<Entry>,
    pub pending: Option<Pending>,
    pub auto_read_only: bool,
    pub capture_output: bool,
    pub ended: bool,
    pub error: Option<String>,
    pub needs_agent_login: bool,
    pub needs_credentials: bool,
    tool_pending: Option<(Uuid, CancellationToken)>,
    continuations: u8,
    pub runtime: std::sync::Arc<crate::integrations::Runtime>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            entries: Vec::new(),
            pending: None,
            auto_read_only: false,
            capture_output: false,
            ended: false,
            error: None,
            needs_agent_login: false,
            needs_credentials: false,
            tool_pending: None,
            continuations: 0,
            runtime: std::sync::Arc::new(crate::integrations::Runtime::new()),
        }
    }
    pub fn begin(
        &mut self,
        prompt: String,
        terminal_revision: u64,
    ) -> Result<(Uuid, CancellationToken)> {
        ensure!(!self.ended, "此终端的 AI 会话已结束");
        ensure!(self.pending.is_none(), "请等待当前回复，或先停止请求");
        ensure!(!self.unresolved_tools(), "请先执行或拒绝待处理工具调用");
        self.continuations = 0;
        ensure!(!prompt.trim().is_empty(), "请输入问题");
        ensure!(
            self.entries.len() < 126
                && self
                    .entries
                    .iter()
                    .map(|entry| entry.turn.text().len())
                    .sum::<usize>()
                    + prompt.len()
                    <= 256 * 1024,
            "对话过长，请新建会话"
        );
        self.error = None;
        self.needs_agent_login = false;
        self.needs_credentials = false;
        self.entries.push(Entry {
            turn: Turn::User(prompt),
            commands: Vec::new(),
            tools: Vec::new(),
        });
        let id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.pending = Some(Pending {
            id,
            cancel: cancel.clone(),
            terminal_revision,
            partial: String::new(),
        });
        Ok((id, cancel))
    }
    pub fn user_message(&self, session: Uuid, index: usize) -> Option<&str> {
        if self.ended || self.id != session {
            return None;
        }
        self.entries.get(index).and_then(|entry| match &entry.turn {
            Turn::User(text) => Some(text.as_str()),
            _ => None,
        })
    }
    pub fn progress(&mut self, session: Uuid, request: Uuid, text: String) -> bool {
        if self.ended || session != self.id || text.len() > 256 * 1024 {
            return false;
        }
        let Some(pending) = &mut self.pending else {
            return false;
        };
        if pending.id != request || pending.cancel.is_cancelled() {
            return false;
        }
        pending.partial = text;
        true
    }
    pub fn accept(&mut self, session: Uuid, request: Uuid, reply: Result<Reply>) -> Option<u64> {
        if self.ended
            || self.id != session
            || self
                .pending
                .as_ref()
                .is_none_or(|pending| pending.id != request)
        {
            return None;
        }
        let pending = self.pending.take()?;
        if pending.cancel.is_cancelled() {
            return None;
        }
        let blocks = reply
            .as_ref()
            .map(|reply| crate::command::proposal_blocks(&reply.text))
            .unwrap_or_default();
        match reply {
            Ok(reply) => self.entries.push(Entry {
                turn: if let Some(message) = reply.message {
                    Turn::Protocol {
                        text: reply.text,
                        message,
                    }
                } else {
                    Turn::Assistant(reply.text)
                },
                tools: reply.tools,
                commands: reply
                    .commands
                    .into_iter()
                    .map(|text| ProposedCommand {
                        id: Uuid::new_v4(),
                        language: blocks
                            .iter()
                            .find(|block| block.command == text)
                            .map(|block| block.language.clone()),
                        text,
                        state: CommandState::Suggested,
                    })
                    .collect(),
            }),
            Err(error) => {
                self.keep_partial(pending.partial);
                self.needs_agent_login =
                    error.downcast_ref::<crate::acp::LoginRequired>().is_some();
                self.needs_credentials = error
                    .downcast_ref::<crate::config::CredentialError>()
                    .is_some();
                self.error = Some(format!("{error:#}"));
            }
        }
        Some(pending.terminal_revision)
    }
    pub fn stop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancel.cancel();
            if !pending.partial.is_empty() {
                self.error = Some("已停止回复".into());
            }
            self.keep_partial(pending.partial);
        }
        if let Some((id, cancel)) = self.tool_pending.take() {
            cancel.cancel();
            let _ = self.finish_tool(self.id, id, Err(anyhow::anyhow!("工具调用已取消")));
        }
    }
    fn keep_partial(&mut self, text: String) {
        if !text.is_empty() {
            self.entries.push(Entry {
                turn: Turn::Assistant(text),
                commands: Vec::new(),
                tools: Vec::new(),
            });
        }
    }
    pub fn unresolved_tools(&self) -> bool {
        self.entries
            .iter()
            .flat_map(|entry| &entry.tools)
            .any(|tool| {
                matches!(
                    tool.state,
                    crate::integrations::ToolState::Suggested
                        | crate::integrations::ToolState::Running
                )
            })
    }
    pub fn begin_tool(&mut self, id: Uuid) -> Result<CancellationToken> {
        ensure!(
            !self.ended && self.pending.is_none() && self.tool_pending.is_none(),
            "请等待当前任务结束"
        );
        let tool = self
            .entries
            .iter_mut()
            .flat_map(|entry| &mut entry.tools)
            .find(|tool| tool.id == id)
            .ok_or_else(|| anyhow::anyhow!("工具调用不存在"))?;
        ensure!(
            tool.state == crate::integrations::ToolState::Suggested,
            "此工具调用已处理"
        );
        tool.state = crate::integrations::ToolState::Running;
        let cancel = CancellationToken::new();
        self.tool_pending = Some((id, cancel.clone()));
        Ok(cancel)
    }
    pub fn reject_tool(&mut self, id: Uuid) -> bool {
        if self.tool_pending.is_some() || self.pending.is_some() {
            return false;
        }
        self.finish_tool(self.id, id, Err(anyhow::anyhow!("用户拒绝执行此工具")))
    }
    pub fn finish_tool(&mut self, session: Uuid, id: Uuid, result: Result<String>) -> bool {
        if self.ended || session != self.id {
            return false;
        }
        let Some(tool) = self
            .entries
            .iter_mut()
            .flat_map(|entry| &mut entry.tools)
            .find(|tool| tool.id == id)
        else {
            return false;
        };
        if !matches!(
            tool.state,
            crate::integrations::ToolState::Suggested | crate::integrations::ToolState::Running
        ) {
            return false;
        }
        let rejected = tool.state == crate::integrations::ToolState::Suggested;
        if let Some((pending, cancel)) = &self.tool_pending
            && (*pending != id || cancel.is_cancelled())
        {
            return false;
        }
        self.tool_pending = None;
        let text = match result {
            Ok(text) => {
                tool.state = crate::integrations::ToolState::Completed;
                text
            }
            Err(error) => {
                let text = format!("{error:#}");
                tool.state = if rejected {
                    crate::integrations::ToolState::Rejected
                } else {
                    crate::integrations::ToolState::Failed(text.clone())
                };
                serde_json::json!({"error":text}).to_string()
            }
        };
        let message = rig_core::completion::Message::tool_result(
            tool.call.id.clone(),
            tool.call.function.name.clone(),
            text.clone(),
        );
        self.entries.push(Entry {
            turn: Turn::Protocol { text, message },
            commands: Vec::new(),
            tools: Vec::new(),
        });
        true
    }
    pub fn continue_tools(&mut self, terminal_revision: u64) -> Result<(Uuid, CancellationToken)> {
        ensure!(
            !self.ended
                && self.pending.is_none()
                && self.tool_pending.is_none()
                && !self.unresolved_tools(),
            "工具尚未完成"
        );
        ensure!(self.continuations < 8, "工具轮次已达上限，请输入新的问题");
        self.continuations += 1;
        let id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.pending = Some(Pending {
            id,
            cancel: cancel.clone(),
            terminal_revision,
            partial: String::new(),
        });
        Ok((id, cancel))
    }
    pub fn tool_running(&self) -> bool {
        self.tool_pending.is_some()
    }

    pub fn reset(&mut self) {
        self.stop();
        self.runtime.cancel.cancel();
        self.runtime = std::sync::Arc::new(crate::integrations::Runtime::new());
        self.id = Uuid::new_v4();
        self.entries.clear();
        self.error = None;
        self.needs_agent_login = false;
        self.needs_credentials = false;
    }
    pub fn end(&mut self) {
        self.stop();
        self.runtime.cancel.cancel();
        self.ended = true;
    }
    pub fn conversation(&self) -> Vec<Turn> {
        self.entries
            .iter()
            .map(|entry| entry.turn.clone())
            .collect()
    }
    pub fn command_mut(&mut self, id: Uuid) -> Option<&mut ProposedCommand> {
        self.entries
            .iter_mut()
            .flat_map(|entry| &mut entry.commands)
            .find(|command| command.id == id)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
        self.runtime.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_replies_check_language_terminal_state_and_limits_before_dispatch() -> Result<()> {
        let environment = crate::session_environment::Environment::from_current_shell_probe(
            b"NUTTY_ENV_V1\nbash\nLinux\n6.8.0\nx86_64\n",
        )
        .context("Linux target")?;
        let diagnostic = "uname -srm; printf 'shell=%s\\n' \"$0\"; pwd; ls -la | head -n 40";
        let text = format!(
            "```bash\n{diagnostic}\n```\n```powershell\npwd\n```\n```sh\ncat\n```\n```sh\nrm file\n```\n```sh\nuname -s\n```\n```sh\nwhoami\n```\n```sh\nid\n```"
        );
        let mut session = Session::new();
        let (request, _) = session.begin("inspect target".into(), 7)?;
        session
            .accept(
                session.id,
                request,
                Ok(Reply {
                    commands: crate::command::proposals(&text),
                    text,
                    tools: vec![],
                    message: None,
                }),
            )
            .context("reply")?;
        let entry = session.entries.last_mut().context("entry")?;
        for state in [
            AutomationState::ShellUnavailable,
            AutomationState::TerminalChanged,
        ] {
            let mut pending = entry.clone();
            assert!(pending.prepare_automatic(&environment, state).is_empty());
            assert!(
                matches!(&pending.commands[0].state, CommandState::Deferred(reason) if !reason.is_empty())
            );
        }
        let dispatch = entry.prepare_automatic(&environment, AutomationState::Ready);
        assert_eq!(dispatch.len(), 3);
        assert_eq!(
            dispatch[0].1,
            crate::command::prepare_read_only(diagnostic)?
        );
        for index in [1, 2, 3, 6] {
            assert!(
                !dispatch
                    .iter()
                    .any(|(id, _)| *id == entry.commands[index].id)
            );
            assert!(
                matches!(&entry.commands[index].state, CommandState::Deferred(reason) if !reason.is_empty())
            );
        }
        assert_eq!(entry.commands[1].language.as_deref(), Some("powershell"));
        for (id, _) in dispatch {
            entry
                .commands
                .iter_mut()
                .find(|command| command.id == id)
                .context("dispatched command")?
                .state = CommandState::Sent;
        }
        let mut already_sent = entry.clone();
        already_sent
            .commands
            .retain(|command| command.state == CommandState::Sent);
        assert!(
            already_sent
                .prepare_automatic(&environment, AutomationState::Ready)
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn user_message_reuse_is_exact_and_rejects_other_sessions_and_assistant_entries() -> Result<()>
    {
        let mut session = Session::new();
        let original = session.id;
        let text = "  排查磁盘\n保留多行内容  ";
        let (request, _) = session.begin(text.into(), 1)?;
        session.accept(
            original,
            request,
            Err(crate::config::CredentialError::Unavailable.into()),
        );
        assert!(session.needs_credentials);
        assert_eq!(session.user_message(original, 0), Some(text));
        assert_eq!(session.user_message(Uuid::new_v4(), 0), None);
        session.entries.push(Entry {
            turn: Turn::Assistant("reply".into()),
            commands: vec![],
            tools: vec![],
        });
        assert_eq!(session.user_message(original, 1), None);
        session.reset();
        assert!(!session.needs_credentials);
        session.begin("new conversation".into(), 1)?;
        assert_eq!(session.user_message(original, 0), None);
        session.end();
        assert_eq!(session.user_message(session.id, 0), None);
        Ok(())
    }

    #[test]
    fn inline_execution_matches_only_final_shell_proposals_and_clears_login_on_reset() -> Result<()>
    {
        let mut session = Session::new();
        let (request, _) = session.begin("show commands".into(), 1)?;
        session
            .accept(
                session.id,
                request,
                Ok(Reply {
                    text: "```sh\npwd\n```\n```json\npwd\n```".into(),
                    commands: vec!["pwd".into()],
                    tools: vec![],
                    message: None,
                }),
            )
            .context("reply should be accepted")?;
        let proposal = &session.entries[1].commands[0];
        assert!(proposal.matches_block(Some("sh"), "pwd\n"));
        assert!(proposal.matches_block(Some("SH"), "\npwd\n"));
        assert!(!proposal.matches_block(Some("powershell"), "pwd"));
        assert!(!proposal.matches_block(Some("json"), "pwd"));
        assert!(!proposal.matches_block(None, "pwd"));
        assert!(!proposal.matches_block(Some("sh"), "pwd && whoami"));
        let (request, _) = session.begin("connect Agent".into(), 1)?;
        session.accept(session.id, request, Err(crate::acp::LoginRequired.into()));
        assert!(session.needs_agent_login);
        session.reset();
        assert!(!session.needs_agent_login);
        Ok(())
    }
    #[test]
    fn tool_stop_and_reset_reject_late_results_and_cancel_the_session_runtime() -> Result<()> {
        let mut session = Session::new();
        let tool_id = Uuid::new_v4();
        let call: rig_core::completion::message::ToolCall = serde_json::from_value(
            serde_json::json!({ "id": {"provider": "call-1"}, "function": { "name": "nutty_list_skills", "arguments": {} } }),
        )?;
        session.entries.push(Entry {
            turn: Turn::Assistant("Inspect skills".into()),
            commands: Vec::new(),
            tools: vec![crate::integrations::ProposedTool {
                id: tool_id,
                call,
                target: crate::integrations::Target::ListSkills,
                state: crate::integrations::ToolState::Suggested,
            }],
        });
        let cancel = session.begin_tool(tool_id)?;
        session.stop();
        assert!(cancel.is_cancelled());
        assert!(!session.unresolved_tools());
        assert!(!session.finish_tool(session.id, tool_id, Ok("late result".into())));
        let original = session.id;
        let runtime = session.runtime.clone();
        session.reset();
        assert!(runtime.cancel.is_cancelled());
        assert!(!session.finish_tool(original, tool_id, Ok("stale result".into())));
        assert!(session.entries.is_empty());
        assert!(!session.runtime.cancel.is_cancelled());
        Ok(())
    }

    #[test]
    fn stopped_streams_keep_text_without_creating_commands_or_accepting_late_progress() -> Result<()>
    {
        let mut session = Session::new();
        let id = session.id;
        let (request, cancelled) = session.begin("inspect".into(), 3)?;
        assert!(session.progress(id, request, "```sh\npwd\n```".into()));
        session.stop();
        assert!(cancelled.is_cancelled());
        assert!(!session.progress(id, request, "stale".into()));
        assert_eq!(
            session.entries.last().context("partial entry")?.turn.text(),
            "```sh\npwd\n```"
        );
        assert!(
            session
                .entries
                .last()
                .context("partial entry")?
                .commands
                .is_empty()
        );
        let (request, _) = session.begin("next".into(), 4)?;
        session.reset();
        assert!(!session.progress(id, request, "stale after reset".into()));
        assert!(session.entries.is_empty());
        Ok(())
    }
    #[test]
    fn reset_close_and_pane_switch_preserve_request_ownership() -> Result<()> {
        let mut first = Session::new();
        let mut second = Session::new();
        let old_session = first.id;
        let (old_request, cancelled) = first.begin("first pane".into(), 7)?;
        second.begin("second pane".into(), 8)?;
        first.reset();
        assert!(cancelled.is_cancelled());
        assert!(
            first
                .accept(
                    old_session,
                    old_request,
                    Ok(Reply {
                        text: "stale".into(),
                        commands: vec!["pwd".into()],
                        tools: vec![],
                        message: None
                    })
                )
                .is_none()
        );
        assert!(first.entries.is_empty());
        assert_eq!(second.conversation()[0].text(), "second pane");
        let (_, cancelled) = first.begin("fresh".into(), 9)?;
        first.end();
        assert!(cancelled.is_cancelled());
        assert!(first.begin("closed".into(), 10).is_err());
        let cancelled = second
            .pending
            .as_ref()
            .expect("request still belongs to second pane")
            .cancel
            .clone();
        drop(second);
        assert!(cancelled.is_cancelled());
        Ok(())
    }
}
