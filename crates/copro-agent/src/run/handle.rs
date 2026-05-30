use super::{
    AgentCheckpoint, AgentControl, AgentControlKind, AgentControlSignal, AgentOutcome,
    AgentRunState, AgentStepId, AgentStepReport,
};
use crate::cancel::RunCancellation;
use crate::context::AgentStreamItem;
use crate::event::{AgentEvent, AgentStream};
use copro_api::error::{Error, Result};
use copro_api::message::{OutputContent, ToolCall, ToolCallId, ToolResult};
use copro_api::response::FinishReason;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, mpsc, oneshot};

#[derive(Clone)]
pub struct AgentRunHandle {
    events: Arc<Mutex<mpsc::Receiver<AgentStreamItem>>>,
    state: Arc<Mutex<AgentRunHandleState>>,
    cancellation: RunCancellation,
    driver_active: Arc<AtomicBool>,
}

struct AgentRunDriverLease {
    active: Arc<AtomicBool>,
}

impl Drop for AgentRunDriverLease {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

struct AgentRunHandleState {
    pending_control: Option<(AgentStepId, oneshot::Sender<AgentControlSignal>)>,
    pending_resume: Option<(AgentStepId, oneshot::Sender<()>)>,
    pause_requested: bool,
    preempt_requested: bool,
    last_report: Option<AgentStepReport>,
    state: Option<AgentRunState>,
    active_tool_call_ids: HashSet<ToolCallId>,
}

impl AgentRunHandle {
    pub(crate) fn new(
        events: mpsc::Receiver<AgentStreamItem>,
        cancellation: RunCancellation,
    ) -> Self {
        Self {
            events: Arc::new(Mutex::new(events)),
            state: Arc::new(Mutex::new(AgentRunHandleState {
                pending_control: None,
                pending_resume: None,
                pause_requested: false,
                preempt_requested: false,
                last_report: None,
                state: None,
                active_tool_call_ids: HashSet::new(),
            })),
            cancellation,
            driver_active: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn into_stream(self) -> AgentStream {
        Box::pin(async_stream::try_stream! {
            let _lease = self.try_acquire_driver()?;
            while let Some(event) = self.next_event().await? {
                yield event;
            }
        })
    }

    pub fn events(self) -> AgentStream {
        self.into_stream()
    }

    pub async fn step(&self) -> Result<AgentStepReport> {
        let _lease = self.try_acquire_driver()?;
        self.state.lock().await.continue_pending_control();

        let mut events = Vec::new();
        loop {
            let item = self
                .events
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| Error::client("agent run finished"))?;

            match item {
                AgentStreamItem::Event(event) => {
                    let event = *event;
                    events.push(event.clone());
                    self.state.lock().await.observe_event(&event);
                }
                AgentStreamItem::ControlRequired {
                    step,
                    outcome,
                    reply,
                } => {
                    let step = *step;
                    let outcome = *outcome;
                    events.push(AgentEvent::ControlRequired {
                        step: step.clone(),
                        outcome: outcome.clone(),
                    });
                    let mut state_inner = self.state.lock().await;
                    let mut state = AgentRunState::WaitingControl {
                        step: step.clone(),
                        outcome: outcome.clone(),
                    };
                    state_inner.state = Some(state.clone());
                    if state_inner.preempt_requested {
                        state_inner.preempt_requested = false;
                        state = AgentRunState::Preempting { step_id: step.id };
                        state_inner.state = Some(state.clone());
                        let _ = reply.send(AgentControlSignal::preempt());
                    } else if state_inner.pause_requested {
                        state_inner.pause_requested = false;
                        state = AgentRunState::Paused { at: step.id };
                        state_inner.state = Some(state.clone());
                        let _ = reply.send(state_inner.make_pause_signal(step.id));
                    } else {
                        state_inner.pending_control = Some((step.id, reply));
                    }
                    let report = AgentStepReport {
                        step,
                        outcome,
                        state,
                        events,
                    };
                    state_inner.last_report = Some(report.clone());
                    return Ok(report);
                }
                AgentStreamItem::Error(error) => {
                    self.state.lock().await.state = Some(AgentRunState::Aborted);
                    return Err(error);
                }
            }
        }
    }

    pub async fn step_until_control(&self) -> Result<AgentCheckpoint> {
        let report = self.step().await?;
        Ok(AgentCheckpoint::from_report(report))
    }

    pub async fn control(
        &self,
        step_id: AgentStepId,
        control: AgentControl,
    ) -> Result<AgentStepReport> {
        let mut inner = self.state.lock().await;
        let report = inner
            .last_report
            .clone()
            .ok_or_else(|| Error::client("agent run is not waiting for control"))?;
        let Some((pending_step_id, _)) = &inner.pending_control else {
            return Err(Error::client("agent run is not waiting for control"));
        };
        if *pending_step_id != step_id {
            return Err(Error::client("stale agent control step id"));
        }
        inner.validate_control(&report.outcome, &control)?;
        inner.observe_control(&report.outcome, &control);

        match control {
            AgentControl::Continue => {
                inner.send_pending_control(AgentControl::Continue);
                Ok(report)
            }
            AgentControl::Pause => {
                inner.send_pause_control(step_id);
                inner.state = Some(AgentRunState::Paused { at: step_id });
                Ok(report)
            }
            AgentControl::FinishRun => {
                self.cancellation.cancel();
                inner.send_pending_control(control);
                inner.state = if outcome_requires_recovery(&report.outcome) {
                    Some(AgentRunState::Recovering { after: step_id })
                } else {
                    Some(AgentRunState::Finished)
                };
                Ok(report)
            }
            AgentControl::AbortRun => {
                self.cancellation.cancel();
                inner.send_pending_control(control);
                inner.state = if outcome_requires_recovery(&report.outcome) {
                    Some(AgentRunState::Recovering { after: step_id })
                } else {
                    Some(AgentRunState::Aborted)
                };
                Ok(report)
            }
            other => {
                inner.send_pending_control(other);
                Ok(report)
            }
        }
    }

    pub async fn pause(&self) -> Result<()> {
        let mut inner = self.state.lock().await;
        let Some((step_id, _)) = &inner.pending_control else {
            inner.pause_requested = true;
            return Ok(());
        };
        let step_id = *step_id;
        inner.send_pause_control(step_id);
        inner.state = Some(AgentRunState::Paused { at: step_id });
        Ok(())
    }

    pub async fn resume(&self) -> Result<()> {
        let mut inner = self.state.lock().await;
        if let Some((_, resume)) = inner.pending_resume.take() {
            let _ = resume.send(());
        } else {
            inner.continue_pending_control();
        }
        Ok(())
    }

    pub async fn preempt(&self) -> Result<()> {
        self.cancellation.cancel();
        let mut inner = self.state.lock().await;
        if let Some((step_id, _)) = &inner.pending_control {
            inner.state = Some(AgentRunState::Preempting { step_id: *step_id });
            inner.send_pending_signal(AgentControlSignal::preempt());
        } else {
            inner.preempt_requested = true;
        }
        Ok(())
    }

    pub async fn state(&self) -> Result<AgentRunState> {
        let inner = self.state.lock().await;
        inner
            .state
            .clone()
            .ok_or_else(|| Error::client("agent run has not started"))
    }

    fn try_acquire_driver(&self) -> Result<AgentRunDriverLease> {
        self.driver_active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| AgentRunDriverLease {
                active: Arc::clone(&self.driver_active),
            })
            .map_err(|_| Error::client("agent run already has an active driver"))
    }

    async fn next_event(&self) -> Result<Option<AgentEvent>> {
        self.state.lock().await.continue_pending_control();

        match self.events.lock().await.recv().await {
            Some(AgentStreamItem::Event(event)) => {
                let event = *event;
                let mut state = self.state.lock().await;
                state.observe_event(&event);
                Ok(Some(event))
            }
            Some(AgentStreamItem::ControlRequired {
                step,
                outcome,
                reply,
            }) => {
                let step = *step;
                let outcome = *outcome;
                let event = AgentEvent::ControlRequired {
                    step: step.clone(),
                    outcome,
                };
                let mut state = self.state.lock().await;
                state.observe_event(&event);
                let signal = if state.preempt_requested {
                    state.preempt_requested = false;
                    state.state = Some(AgentRunState::Preempting { step_id: step.id });
                    AgentControlSignal::preempt()
                } else if state.pause_requested {
                    state.pause_requested = false;
                    state.state = Some(AgentRunState::Paused { at: step.id });
                    state.make_pause_signal(step.id)
                } else {
                    AgentControlSignal::continue_run()
                };
                let _ = reply.send(signal);
                Ok(Some(event))
            }
            Some(AgentStreamItem::Error(error)) => {
                self.state.lock().await.state = Some(AgentRunState::Aborted);
                Err(error)
            }
            None => Ok(None),
        }
    }
}

impl AgentRunHandleState {
    fn validate_control(&self, outcome: &AgentOutcome, control: &AgentControl) -> Result<()> {
        if !allowed_controls_for_outcome(outcome).contains(&control.kind()) {
            return Err(Error::client(format!(
                "agent control {:?} is not valid for this step",
                control.kind()
            )));
        }

        match (outcome, control) {
            (
                AgentOutcome::ToolResultCommitted { tool, .. },
                AgentControl::ReplaceToolResult(result),
            ) => validate_tool_result_identity(tool, result),
            (
                AgentOutcome::ToolPlanned { tool, .. } | AgentOutcome::ToolRejected { tool, .. },
                AgentControl::ReplaceToolCall(replacement),
            ) => validate_tool_call_replacement(&self.active_tool_call_ids, tool, replacement),
            (
                AgentOutcome::ModelOutputFinished { reason, .. },
                AgentControl::ReplaceAssistantOutput(content),
            ) => validate_assistant_output_for_reason(*reason, content),
            _ => Ok(()),
        }
    }

    fn observe_control(&mut self, outcome: &AgentOutcome, control: &AgentControl) {
        if let (
            AgentOutcome::ToolPlanned { tool, .. } | AgentOutcome::ToolRejected { tool, .. },
            AgentControl::ReplaceToolCall(replacement),
        ) = (outcome, control)
        {
            self.active_tool_call_ids.remove(&tool.id);
            self.active_tool_call_ids.insert(replacement.id.clone());
        }
    }

    fn continue_pending_control(&mut self) {
        if self.pending_resume.is_none() {
            self.send_pending_control(AgentControl::Continue);
        }
    }

    fn send_pending_control(&mut self, control: AgentControl) {
        self.send_pending_signal(AgentControlSignal::Control(control));
    }

    fn send_pending_signal(&mut self, signal: AgentControlSignal) {
        if let Some((_, ack)) = self.pending_control.take() {
            let _ = ack.send(signal);
        }
    }

    fn send_pause_control(&mut self, step_id: AgentStepId) {
        let Some((_, ack)) = self.pending_control.take() else {
            return;
        };
        let signal = self.make_pause_signal(step_id);
        if ack.send(signal).is_err() {
            self.pending_resume = None;
        }
    }

    fn make_pause_signal(&mut self, step_id: AgentStepId) -> AgentControlSignal {
        let (resume, rx) = oneshot::channel();
        self.pending_resume = Some((step_id, resume));
        AgentControlSignal::Pause { resume: rx }
    }

    fn observe_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::StepReady { step } => {
                self.state = Some(AgentRunState::Ready {
                    next: step.action.clone(),
                    step_id: step.id,
                });
            }
            AgentEvent::StepStarted { step } => {
                self.state = Some(AgentRunState::InFlight { step: step.clone() });
            }
            AgentEvent::ControlRequired { step, outcome } => {
                self.state = Some(AgentRunState::WaitingControl {
                    step: step.clone(),
                    outcome: outcome.clone(),
                });
            }
            AgentEvent::RunPaused { at, .. } => {
                self.state = Some(AgentRunState::Paused { at: *at });
            }
            AgentEvent::RunPreempted { at, .. } => {
                self.state = Some(AgentRunState::Preempting { step_id: *at });
            }
            AgentEvent::RunRecovering { after, .. } => {
                self.state = Some(AgentRunState::Recovering { after: *after });
            }
            AgentEvent::RunFinished { .. } => {
                self.state = Some(AgentRunState::Finished);
            }
            AgentEvent::RunAborted { .. } => {
                self.state = Some(AgentRunState::Aborted);
            }
            AgentEvent::AssistantCommitted { content, .. } => {
                self.active_tool_call_ids = content
                    .iter()
                    .filter_map(|item| match item {
                        OutputContent::ToolCall(tool) => Some(tool.id.clone()),
                        _ => None,
                    })
                    .collect();
            }
            _ => {}
        }
    }
}

fn allowed_controls_for_outcome(outcome: &AgentOutcome) -> Vec<AgentControlKind> {
    let mut controls = vec![
        AgentControlKind::Continue,
        AgentControlKind::Pause,
        AgentControlKind::FinishRun,
        AgentControlKind::AbortRun,
    ];
    match outcome {
        AgentOutcome::RequestBuilt(_) => controls.push(AgentControlKind::ReplaceRequest),
        AgentOutcome::ModelDelta { .. } => {
            controls.push(AgentControlKind::ReplaceModelDelta);
            controls.push(AgentControlKind::DropModelDelta);
        }
        AgentOutcome::ModelOutputFinished { .. } => {
            controls.push(AgentControlKind::ReplaceAssistantOutput);
        }
        AgentOutcome::ToolPlanned { .. } | AgentOutcome::ToolRejected { .. } => {
            controls.push(AgentControlKind::ReplaceToolCall);
            controls.push(AgentControlKind::RejectToolCall);
        }
        AgentOutcome::ToolResultCommitted { .. } => {
            controls.push(AgentControlKind::ReplaceToolResult);
        }
        _ => {}
    }
    controls
}

fn validate_tool_result_identity(tool: &ToolCall, result: &ToolResult) -> Result<()> {
    if result.call_id.as_str() != tool.id.as_str() || result.name.as_str() != tool.name.as_str() {
        return Err(Error::client(
            "replacement tool result must keep the original call_id and name",
        ));
    }
    Ok(())
}

fn validate_tool_call_replacement(
    active_tool_call_ids: &HashSet<ToolCallId>,
    original: &ToolCall,
    replacement: &ToolCall,
) -> Result<()> {
    if replacement.id != original.id && active_tool_call_ids.contains(&replacement.id) {
        return Err(Error::client(
            "replacement tool call id must be unique in the current turn",
        ));
    }
    Ok(())
}

fn validate_assistant_output_for_reason(
    reason: FinishReason,
    content: &[OutputContent],
) -> Result<()> {
    let has_tool_call = content
        .iter()
        .any(|item| matches!(item, OutputContent::ToolCall(_)));
    match reason {
        FinishReason::Stop if has_tool_call => Err(Error::client(
            "assistant output with finish reason Stop cannot contain tool calls",
        )),
        FinishReason::ToolCalls if !has_tool_call => Err(Error::client(
            "assistant output with finish reason ToolCalls must contain a tool call",
        )),
        _ => Ok(()),
    }
}

fn outcome_requires_recovery(outcome: &AgentOutcome) -> bool {
    match outcome {
        AgentOutcome::AssistantCommitted { content, .. } => content
            .iter()
            .any(|item| matches!(item, OutputContent::ToolCall(_))),
        AgentOutcome::ToolPlanned { .. }
        | AgentOutcome::ToolRejected { .. }
        | AgentOutcome::ToolStarted { .. }
        | AgentOutcome::ToolFinished { .. }
        | AgentOutcome::ToolResultCommitted { .. } => true,
        _ => false,
    }
}
