//! Pane-owned AI conversations with cancellation, stale-response rejection and command state.

use crate::{
    ai::{Reply, Turn},
    command_review::{self, Verdict},
    i18n,
};
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
    /// Waiting for the model's read-only verdict. A click still sends it at once.
    Reviewing,
    Sent,
    Deferred(String),
}

#[derive(Clone, Debug)]
pub struct ProposedCommand {
    pub id: Uuid,
    pub text: String,
    pub language: Option<String>,
    pub state: CommandState,
    /// The pane's terminal target (its `user@host` session) when the question was
    /// asked; `None` when the terminal reported none.
    pub target: Option<Uuid>,
}

impl ProposedCommand {
    /// Whether the proposal may run on `current`, the pane's terminal target now. A
    /// proposal made for another target is bound to `current` and needs a second,
    /// deliberate request, so a host change in the pane never redirects it silently.
    pub fn retarget(&mut self, current: Option<Uuid>) -> bool {
        if self.target == current {
            return true;
        }
        self.target = current;
        false
    }
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

/// Commands sent without a click after one reply.
pub const MAX_AUTOMATIC_COMMANDS: usize = 3;

/// A read-only review the application has to run: `commands` go to the model, and its
/// verdicts come back through [`Session::finish_review`] under the same `id`.
pub struct Review {
    pub id: Uuid,
    pub cancel: CancellationToken,
    pub commands: Vec<String>,
}

struct ReviewPending {
    id: Uuid,
    cancel: CancellationToken,
    terminal_revision: u64,
    commands: Vec<Uuid>,
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
    pub terminal_target: Option<Uuid>,
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
    review: Option<ReviewPending>,
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
            review: None,
            continuations: 0,
            runtime: std::sync::Arc::new(crate::integrations::Runtime::new()),
        }
    }
    pub fn begin(
        &mut self,
        prompt: String,
        terminal_revision: u64,
        terminal_target: Option<Uuid>,
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
        // A new question means the user has moved on from the last reply.
        self.cancel_review();
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
            terminal_target,
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
                        target: pending.terminal_target,
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
    /// Starts the read-only review of the newest reply's commands. Everything that can
    /// be decided without the model is decided here: a command in another shell's
    /// language, a terminal that is not at an idle prompt and oversized commands stay
    /// manual and are never sent to the model. `terminal_revision` is the terminal
    /// state the question was asked in; the commands are sent only if it still holds.
    pub fn begin_review(
        &mut self,
        environment: &crate::session_environment::Environment,
        state: AutomationState,
        terminal_revision: u64,
    ) -> Option<Review> {
        self.cancel_review();
        if self.ended || self.pending.is_some() {
            return None;
        }
        let entry = self.entries.last_mut()?;
        let mut ids = Vec::new();
        let mut commands = Vec::new();
        let mut bytes = 0;
        for proposed in &mut entry.commands {
            if proposed.state != CommandState::Suggested {
                continue;
            }
            let result = (|| -> Result<()> {
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
                    proposed.text.len() <= command_review::MAX_COMMAND_BYTES,
                    "命令过长，无法自动校验，请点击执行"
                );
                ensure!(
                    commands.len() < command_review::MAX_COMMANDS
                        && bytes + proposed.text.len() <= command_review::MAX_REVIEW_BYTES,
                    "本次回复的命令过多，其余请点击执行"
                );
                Ok(())
            })();
            match result {
                Ok(()) => {
                    bytes += proposed.text.len();
                    proposed.state = CommandState::Reviewing;
                    ids.push(proposed.id);
                    commands.push(proposed.text.clone());
                }
                Err(error) => {
                    proposed.state = CommandState::Deferred(i18n::localize(error.to_string()))
                }
            }
        }
        if commands.is_empty() {
            return None;
        }
        let id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.review = Some(ReviewPending {
            id,
            cancel: cancel.clone(),
            terminal_revision,
            commands: ids,
        });
        Some(Review {
            id,
            cancel,
            commands,
        })
    }
    /// Applies the model's verdicts. Allowed commands, at most
    /// [`MAX_AUTOMATIC_COMMANDS`], are handed to `send` in reply order together with the
    /// terminal revision of the question; everything else waits for a click with the
    /// reason shown. A command the user sent or that changed meanwhile is left alone.
    /// Returns whether the verdicts belonged to the current review.
    pub fn finish_review(
        &mut self,
        session: Uuid,
        review: Uuid,
        verdicts: Result<Vec<Verdict>>,
        send: impl FnOnce(u64, &[&str]) -> Result<()>,
    ) -> bool {
        if self.ended
            || self.id != session
            || self
                .review
                .as_ref()
                .is_none_or(|pending| pending.id != review || pending.cancel.is_cancelled())
        {
            return false;
        }
        let Some(pending) = self.review.take() else {
            return false;
        };
        let verdicts = verdicts.and_then(|verdicts| {
            ensure!(
                verdicts.len() == pending.commands.len(),
                "模型没有返回可识别的只读判定"
            );
            Ok(verdicts)
        });
        let verdicts = match verdicts {
            Ok(verdicts) => verdicts,
            Err(error) => {
                let detail: String = format!("{error:#}").chars().take(240).collect();
                let reason = i18n::text("只读校验未完成，请点击执行：{}").replacen(
                    "{}",
                    &i18n::localize(detail),
                    1,
                );
                self.defer_reviewing(&pending.commands, &reason);
                return true;
            }
        };
        let mut allowed = Vec::new();
        for (id, verdict) in pending.commands.iter().zip(verdicts) {
            let Some(command) = self
                .command_mut(*id)
                .filter(|command| command.state == CommandState::Reviewing)
            else {
                continue;
            };
            if !verdict.allow {
                command.state = CommandState::Deferred(if verdict.reason.is_empty() {
                    i18n::text("模型判定需手动确认").into()
                } else {
                    i18n::text("模型判定需手动确认：{}").replacen("{}", &verdict.reason, 1)
                });
            } else if allowed.len() == MAX_AUTOMATIC_COMMANDS {
                command.state = CommandState::Deferred(
                    i18n::text("每次最多自动发送 3 条命令，其余请手动执行").into(),
                );
            } else {
                allowed.push((*id, command.text.clone()));
            }
        }
        if allowed.is_empty() {
            return true;
        }
        let texts: Vec<&str> = allowed.iter().map(|(_, text)| text.as_str()).collect();
        let state = match send(pending.terminal_revision, &texts) {
            Ok(()) => CommandState::Sent,
            Err(error) => CommandState::Deferred(i18n::localize(error.to_string())),
        };
        for (id, _) in &allowed {
            if let Some(command) = self.command_mut(*id) {
                command.state = state.clone();
            }
        }
        true
    }
    /// Abandons a running review; its commands wait for a click and a late verdict is
    /// ignored.
    pub fn cancel_review(&mut self) {
        if let Some(pending) = self.review.take() {
            pending.cancel.cancel();
            let reason = i18n::text("只读校验已取消，请点击执行");
            self.defer_reviewing(&pending.commands, reason);
        }
    }
    pub fn reviewing(&self) -> bool {
        self.review.is_some()
    }
    fn defer_reviewing(&mut self, commands: &[Uuid], reason: &str) {
        for id in commands {
            if let Some(command) = self
                .command_mut(*id)
                .filter(|command| command.state == CommandState::Reviewing)
            {
                command.state = CommandState::Deferred(reason.into());
            }
        }
    }
    pub fn stop(&mut self) {
        self.cancel_review();
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
    pub fn continue_tools(
        &mut self,
        terminal_revision: u64,
        terminal_target: Option<Uuid>,
    ) -> Result<(Uuid, CancellationToken)> {
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
            terminal_target,
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

    fn linux() -> Result<crate::session_environment::Environment> {
        crate::session_environment::Environment::from_current_shell_probe(
            b"NUTTY_ENV_V1\nbash\nLinux\n6.8.0\nx86_64\n",
        )
        .context("Linux target")
    }

    /// A session whose newest reply proposes `blocks`, asked at terminal revision 7.
    fn proposing(blocks: &[(&str, &str)]) -> Result<Session> {
        let text: String = blocks
            .iter()
            .map(|(language, command)| format!("```{language}\n{command}\n```\n"))
            .collect();
        let mut session = Session::new();
        let (request, _) = session.begin("inspect target".into(), 7, None)?;
        let revision = session.accept(
            session.id,
            request,
            Ok(Reply {
                commands: crate::command::proposals(&text),
                text,
                tools: vec![],
                message: None,
            }),
        );
        assert_eq!(revision, Some(7));
        Ok(session)
    }

    fn states(session: &Session) -> Vec<CommandState> {
        session
            .entries
            .last()
            .map(|entry| {
                entry
                    .commands
                    .iter()
                    .map(|command| command.state.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn verdict(allow: bool, reason: &str) -> Verdict {
        Verdict {
            allow,
            reason: reason.into(),
        }
    }

    fn deferred(state: &CommandState, part: &str) -> bool {
        matches!(state, CommandState::Deferred(reason) if reason.contains(part))
    }

    #[test]
    fn reviews_gate_locally_then_send_only_what_the_model_allows() -> Result<()> {
        let environment = linux()?;
        let diagnostic = "kubectl get pods -A | awk '$4 != \"Running\"'";
        let blocks = [
            ("bash", diagnostic),
            ("powershell", "Get-Process"),
            ("sh", "rm file"),
            ("sh", "docker ps"),
            ("sh", "go env"),
            ("sh", "id"),
        ];
        for state in [
            AutomationState::ShellUnavailable,
            AutomationState::TerminalChanged,
        ] {
            let mut session = proposing(&blocks)?;
            assert!(
                session.begin_review(&environment, state, 7).is_none(),
                "nothing is reviewed while the terminal cannot take a command"
            );
            assert!(!session.reviewing());
            assert!(states(&session).iter().all(|state| deferred(state, "")));
        }
        let mut session = proposing(&blocks)?;
        let id = session.id;
        let review = session
            .begin_review(&environment, AutomationState::Ready, 7)
            .context("review")?;
        assert_eq!(
            review.commands,
            [diagnostic, "rm file", "docker ps", "go env", "id"],
            "any program reaches the model; only the other shell's block is held back"
        );
        let reviewing = states(&session);
        assert!(deferred(&reviewing[1], "Shell"));
        for index in [0, 2, 3, 4, 5] {
            assert_eq!(reviewing[index], CommandState::Reviewing);
        }
        // Verdicts for another session or review are not this review's verdicts.
        for (session_id, review_id) in [(Uuid::new_v4(), review.id), (id, Uuid::new_v4())] {
            assert!(!session.finish_review(
                session_id,
                review_id,
                Ok(vec![verdict(true, ""); 5]),
                |_, _| panic!("stale verdicts must not send"),
            ));
        }
        let mut sent = Vec::new();
        assert!(session.finish_review(
            id,
            review.id,
            Ok(vec![
                verdict(true, "lists pods"),
                verdict(false, "deletes a file"),
                verdict(true, "lists containers"),
                verdict(true, "prints settings"),
                verdict(true, "prints the user"),
            ]),
            |revision, commands| {
                assert_eq!(
                    revision, 7,
                    "sent against the terminal state of the question"
                );
                sent = commands.iter().map(|command| command.to_string()).collect();
                Ok(())
            },
        ));
        assert_eq!(
            sent,
            [diagnostic, "docker ps", "go env"],
            "unchanged text, reply order"
        );
        let finished = states(&session);
        for index in [0, 3, 4] {
            assert_eq!(finished[index], CommandState::Sent);
        }
        assert!(
            deferred(&finished[2], "deletes a file"),
            "the model's reason is shown"
        );
        assert!(
            deferred(&finished[5], "3"),
            "the fourth allowed command waits for a click"
        );
        assert!(!session.reviewing());
        assert!(
            !session.finish_review(id, review.id, Ok(vec![verdict(true, ""); 5]), |_, _| {
                panic!("a review is applied once")
            }),
        );
        assert!(
            session
                .begin_review(&environment, AutomationState::Ready, 7)
                .is_none(),
            "sent and refused commands are not reviewed again"
        );
        Ok(())
    }

    #[test]
    fn failed_reviews_and_refused_sends_leave_every_command_manual() -> Result<()> {
        let environment = linux()?;
        let blocks = [("sh", "uptime"), ("sh", "df -h")];
        // The model could not be asked or did not answer in the expected form.
        for failure in [
            Err(anyhow::anyhow!("只读校验超时")),
            Ok(vec![verdict(true, "")]),
            Ok(vec![verdict(true, ""); 3]),
        ] {
            let mut session = proposing(&blocks)?;
            let review = session
                .begin_review(&environment, AutomationState::Ready, 7)
                .context("review")?;
            assert!(
                session.finish_review(session.id, review.id, failure, |_, _| {
                    panic!("an unfinished review must not send")
                })
            );
            assert!(
                states(&session)
                    .iter()
                    .all(|state| deferred(state, "只读校验未完成"))
            );
        }
        // The terminal refused the submission.
        let mut session = proposing(&blocks)?;
        let review = session
            .begin_review(&environment, AutomationState::Ready, 7)
            .context("review")?;
        assert!(session.finish_review(
            session.id,
            review.id,
            Ok(vec![verdict(true, ""); 2]),
            |_, _| anyhow::bail!("终端状态已变化，请手动执行"),
        ));
        assert!(
            states(&session)
                .iter()
                .all(|state| deferred(state, "终端状态已变化"))
        );
        // A command the user clicked during the review is not sent a second time.
        let mut session = proposing(&blocks)?;
        let review = session
            .begin_review(&environment, AutomationState::Ready, 7)
            .context("review")?;
        session.entries[1].commands[0].state = CommandState::Sent;
        let mut sent = Vec::new();
        assert!(session.finish_review(
            session.id,
            review.id,
            Ok(vec![verdict(true, ""); 2]),
            |_, commands| {
                sent = commands.iter().map(|command| command.to_string()).collect();
                Ok(())
            },
        ));
        assert_eq!(sent, ["df -h"]);
        // Oversized commands never leave the application.
        let long = format!("echo {}", "x".repeat(command_review::MAX_COMMAND_BYTES));
        let mut session = proposing(&[("sh", &long), ("sh", "uptime")])?;
        let review = session
            .begin_review(&environment, AutomationState::Ready, 7)
            .context("review")?;
        assert_eq!(review.commands, ["uptime"]);
        assert!(deferred(&states(&session)[0], "过长"));
        Ok(())
    }

    #[test]
    fn a_new_question_stop_reset_and_end_abandon_the_review() -> Result<()> {
        let environment = linux()?;
        type Abandon = fn(&mut Session) -> Result<()>;
        let abandon: [(&str, Abandon); 5] = [
            ("question", |session| {
                session.begin("next".into(), 8, None).map(|_| ())
            }),
            ("stop", |session| {
                session.stop();
                Ok(())
            }),
            ("option off", |session| {
                session.cancel_review();
                Ok(())
            }),
            ("reset", |session| {
                session.reset();
                Ok(())
            }),
            ("end", |session| {
                session.end();
                Ok(())
            }),
        ];
        for (name, abandon) in abandon {
            let mut session = proposing(&[("sh", "uptime")])?;
            let id = session.id;
            let review = session
                .begin_review(&environment, AutomationState::Ready, 7)
                .context("review")?;
            abandon(&mut session)?;
            assert!(
                review.cancel.is_cancelled(),
                "{name} cancels the model request"
            );
            assert!(!session.reviewing());
            if let Some(command) = session
                .entries
                .get(1)
                .and_then(|entry| entry.commands.first())
            {
                assert!(deferred(&command.state, "已取消"), "{name}");
            }
            assert!(
                !session.finish_review(id, review.id, Ok(vec![verdict(true, "")]), |_, _| {
                    panic!("{name}: a late verdict must not send")
                }),
            );
        }
        let mut session = proposing(&[("sh", "uptime")])?;
        let review = session
            .begin_review(&environment, AutomationState::Ready, 7)
            .context("review")?;
        let cancel = review.cancel.clone();
        drop(session);
        assert!(
            cancel.is_cancelled(),
            "closing the pane cancels the model request"
        );
        Ok(())
    }

    #[test]
    fn user_message_reuse_is_exact_and_rejects_other_sessions_and_assistant_entries() -> Result<()>
    {
        let mut session = Session::new();
        let original = session.id;
        let text = "  排查磁盘\n保留多行内容  ";
        let (request, _) = session.begin(text.into(), 1, None)?;
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
        session.begin("new conversation".into(), 1, None)?;
        assert_eq!(session.user_message(original, 0), None);
        session.end();
        assert_eq!(session.user_message(session.id, 0), None);
        Ok(())
    }

    #[test]
    fn inline_execution_matches_only_final_shell_proposals_and_clears_login_on_reset() -> Result<()>
    {
        let mut session = Session::new();
        let (request, _) = session.begin("show commands".into(), 1, None)?;
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
        let (request, _) = session.begin("connect Agent".into(), 1, None)?;
        session.accept(session.id, request, Err(crate::acp::LoginRequired.into()));
        assert!(session.needs_agent_login);
        session.reset();
        assert!(!session.needs_agent_login);
        Ok(())
    }
    #[test]
    fn proposals_remember_the_asked_target_and_need_a_second_request_elsewhere() -> Result<()> {
        let staging = Some(Uuid::new_v4());
        let production = Some(Uuid::new_v4());
        let mut session = Session::new();
        let (request, _) = session.begin("restart nginx".into(), 1, staging)?;
        session
            .accept(
                session.id,
                request,
                Ok(Reply {
                    text: "```sh\nsystemctl restart nginx\n```".into(),
                    commands: vec!["systemctl restart nginx".into()],
                    tools: vec![],
                    message: None,
                }),
            )
            .context("reply should be accepted")?;
        let proposal = session.entries[1].commands[0].id;
        let command = session.command_mut(proposal).context("proposal")?;
        assert_eq!(
            command.target, staging,
            "the target of the question is kept"
        );
        assert!(command.retarget(staging));
        assert!(
            !command.retarget(production),
            "another host must not run the proposal on the same click"
        );
        assert!(
            command.retarget(production),
            "a second deliberate request runs it"
        );
        assert!(
            !command.retarget(None),
            "leaving the SSH target counts as a change"
        );
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
        let (request, cancelled) = session.begin("inspect".into(), 3, None)?;
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
        let (request, _) = session.begin("next".into(), 4, None)?;
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
        let (old_request, cancelled) = first.begin("first pane".into(), 7, None)?;
        second.begin("second pane".into(), 8, None)?;
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
        let (_, cancelled) = first.begin("fresh".into(), 9, None)?;
        first.end();
        assert!(cancelled.is_cancelled());
        assert!(first.begin("closed".into(), 10, None).is_err());
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
