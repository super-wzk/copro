use super::{
    AgentCheckpoint, AgentControl, AgentControlKind, AgentControlSignal, AgentOutcome, AgentStep,
    AgentStepId, AgentStepReport, AgentStreamItem, AgentTurnState,
};
use crate::cancel::TurnCancellation;
use crate::event::{AgentEvent, AgentStream};
use crate::history::AgentHistory;
use copro_api::error::{Error, Result};
use copro_api::message::{OutputContent, ToolCall, ToolCallId, ToolResult};
use copro_api::response::FinishReason;
use std::collections::HashSet;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};

#[must_use = "agent control points should be explicitly continued or controlled; Drop only best-effort continues"]
pub struct AgentControlPoint {
    turn: AgentTurnHandle,
    checkpoint: AgentCheckpoint,
    step_id: AgentStepId,
    _driver: AgentTurnDriverLease,
    controlled: bool,
}

#[derive(Clone)]
pub struct AgentTurnHandle {
    events: Arc<Mutex<mpsc::Receiver<AgentStreamItem>>>,
    state: Arc<Mutex<AgentTurnHandleState>>,
    cancellation: TurnCancellation,
    driver_active: Arc<AtomicBool>,
    completion: AgentTurnCompletion,
}

#[derive(Clone)]
pub(crate) struct AgentTurnCompletion {
    inner: Arc<AgentTurnCompletionState>,
}

struct AgentTurnCompletionState {
    history: Mutex<Option<AgentHistory>>,
    ready: Notify,
}

struct AgentTurnDriverLease {
    active: Arc<AtomicBool>,
}

struct PendingControl {
    step_id: AgentStepId,
    report: AgentStepReport,
    reply: oneshot::Sender<AgentControlSignal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferredControlRequest {
    Pause,
    Preempt,
}

impl Drop for AgentTurnDriverLease {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

struct AgentTurnHandleState {
    pending_control: Option<PendingControl>,
    pending_resume: Option<(AgentStepId, oneshot::Sender<()>)>,
    deferred_control: Option<DeferredControlRequest>,
    state: Option<AgentTurnState>,
    active_tool_call_ids: HashSet<ToolCallId>,
}

impl AgentTurnHandle {
    pub(crate) fn new(
        events: mpsc::Receiver<AgentStreamItem>,
        cancellation: TurnCancellation,
        completion: AgentTurnCompletion,
    ) -> Self {
        Self {
            events: Arc::new(Mutex::new(events)),
            state: Arc::new(Mutex::new(AgentTurnHandleState {
                pending_control: None,
                pending_resume: None,
                deferred_control: None,
                state: None,
                active_tool_call_ids: HashSet::new(),
            })),
            cancellation,
            driver_active: Arc::new(AtomicBool::new(false)),
            completion,
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

    pub async fn step_until_control(&self) -> Result<AgentControlPoint> {
        let lease = self.try_acquire_driver()?;
        self.state.lock().await.continue_pending_control();

        let mut events = Vec::new();
        loop {
            let item = self
                .events
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| Error::client("agent turn finished"))?;

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
                    let report = self
                        .state
                        .lock()
                        .await
                        .enter_manual_control_boundary(step, outcome, events, reply);
                    return Ok(AgentControlPoint::new(self.clone(), report, lease));
                }
                AgentStreamItem::Error(error) => {
                    self.state.lock().await.state = Some(AgentTurnState::Aborted);
                    return Err(error);
                }
            }
        }
    }

    async fn control_step(
        &self,
        step_id: AgentStepId,
        control: AgentControl,
    ) -> Result<AgentStepReport> {
        let mut inner = self.state.lock().await;
        let report = {
            let pending = inner
                .pending_control
                .as_ref()
                .ok_or_else(|| Error::client("agent turn is not waiting for control"))?;
            if pending.step_id != step_id {
                return Err(Error::client("stale agent control step id"));
            }
            pending.report.clone()
        };
        inner.validate_control(&report.outcome, &control)?;
        inner.observe_control(&report.outcome, &control);

        match control {
            AgentControl::Continue => {
                inner.send_pending_control(AgentControl::Continue);
                Ok(report)
            }
            AgentControl::Pause => {
                inner.send_pause_control(step_id);
                inner.state = Some(AgentTurnState::Paused { at: step_id });
                Ok(report)
            }
            AgentControl::FinishTurn => {
                self.cancellation.cancel();
                inner.send_pending_control(control);
                inner.state = if outcome_requires_recovery(&report.outcome) {
                    Some(AgentTurnState::Recovering { after: step_id })
                } else {
                    Some(AgentTurnState::Finished)
                };
                Ok(report)
            }
            AgentControl::AbortTurn => {
                self.cancellation.cancel();
                inner.send_pending_control(control);
                inner.state = if outcome_requires_recovery(&report.outcome) {
                    Some(AgentTurnState::Recovering { after: step_id })
                } else {
                    Some(AgentTurnState::Aborted)
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
        let Some(step_id) = inner
            .pending_control
            .as_ref()
            .map(|pending| pending.step_id)
        else {
            inner.defer_pause();
            return Ok(());
        };
        inner.send_pause_control(step_id);
        inner.state = Some(AgentTurnState::Paused { at: step_id });
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
        if let Some(step_id) = inner
            .pending_control
            .as_ref()
            .map(|pending| pending.step_id)
        {
            inner.state = Some(AgentTurnState::Preempting { step_id });
            inner.send_pending_signal(AgentControlSignal::preempt());
        } else {
            inner.defer_preempt();
        }
        Ok(())
    }

    pub async fn state(&self) -> Result<AgentTurnState> {
        let inner = self.state.lock().await;
        inner
            .state
            .clone()
            .ok_or_else(|| Error::client("agent turn has not started"))
    }

    pub async fn into_history(self) -> AgentHistory {
        self.completion.history().await
    }

    fn try_acquire_driver(&self) -> Result<AgentTurnDriverLease> {
        self.driver_active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| AgentTurnDriverLease {
                active: Arc::clone(&self.driver_active),
            })
            .map_err(|_| Error::client("agent turn already has an active driver"))
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
                    outcome: outcome.clone(),
                };
                let mut state = self.state.lock().await;
                state.enter_auto_control_boundary(&step, &outcome, reply);
                Ok(Some(event))
            }
            Some(AgentStreamItem::Error(error)) => {
                self.state.lock().await.state = Some(AgentTurnState::Aborted);
                Err(error)
            }
            None => Ok(None),
        }
    }
}

impl AgentControlPoint {
    fn new(turn: AgentTurnHandle, report: AgentStepReport, driver: AgentTurnDriverLease) -> Self {
        let step_id = report.step.id;
        Self {
            turn,
            checkpoint: AgentCheckpoint::from_report(report),
            step_id,
            _driver: driver,
            controlled: false,
        }
    }

    pub fn checkpoint(&self) -> &AgentCheckpoint {
        &self.checkpoint
    }

    pub fn report(&self) -> &AgentStepReport {
        self.checkpoint.report()
    }

    pub fn step(&self) -> &AgentStep {
        self.checkpoint.step()
    }

    pub fn state(&self) -> &AgentTurnState {
        self.checkpoint.state()
    }

    pub fn events(&self) -> &[AgentEvent] {
        self.checkpoint.events()
    }

    pub fn pending_outcome(&self) -> &AgentOutcome {
        self.checkpoint.pending_outcome()
    }

    pub async fn control(mut self, control: AgentControl) -> Result<AgentStepReport> {
        let result = self.turn.control_step(self.step_id, control).await;
        if result.is_ok() {
            self.controlled = true;
        }
        result
    }

    pub async fn continue_turn(self) -> Result<AgentStepReport> {
        self.control(AgentControl::Continue).await
    }
}

impl fmt::Debug for AgentControlPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentControlPoint")
            .field("checkpoint", &self.checkpoint)
            .field("controlled", &self.controlled)
            .finish_non_exhaustive()
    }
}

impl Deref for AgentControlPoint {
    type Target = AgentStepReport;

    fn deref(&self) -> &Self::Target {
        self.report()
    }
}

impl Drop for AgentControlPoint {
    fn drop(&mut self) {
        if self.controlled {
            return;
        }

        if let Ok(mut state) = self.turn.state.try_lock() {
            state.continue_pending_control();
            return;
        }

        let turn = self.turn.clone();
        let step_id = self.step_id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = turn.control_step(step_id, AgentControl::Continue).await;
            });
        }
    }
}

impl AgentTurnCompletion {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(AgentTurnCompletionState {
                history: Mutex::new(None),
                ready: Notify::new(),
            }),
        }
    }

    pub(crate) async fn complete(&self, history: AgentHistory) {
        *self.inner.history.lock().await = Some(history);
        self.inner.ready.notify_waiters();
    }

    async fn history(&self) -> AgentHistory {
        loop {
            if let Some(history) = self.inner.history.lock().await.clone() {
                return history;
            }
            self.inner.ready.notified().await;
        }
    }
}

impl AgentTurnHandleState {
    fn enter_auto_control_boundary(
        &mut self,
        step: &AgentStep,
        outcome: &AgentOutcome,
        reply: oneshot::Sender<AgentControlSignal>,
    ) {
        self.enter_waiting_control(step, outcome);
        let signal = self
            .take_requested_control_signal(step.id)
            .map_or_else(AgentControlSignal::continue_turn, |(_, signal)| signal);
        let _ = reply.send(signal);
    }

    fn enter_manual_control_boundary(
        &mut self,
        step: AgentStep,
        outcome: AgentOutcome,
        events: Vec<AgentEvent>,
        reply: oneshot::Sender<AgentControlSignal>,
    ) -> AgentStepReport {
        let step_id = step.id;
        let mut state = self.enter_waiting_control(&step, &outcome);
        let requested_signal = self.take_requested_control_signal(step_id);
        if let Some((requested_state, _)) = &requested_signal {
            state = requested_state.clone();
        }

        let report = AgentStepReport {
            step,
            outcome,
            state,
            events,
        };

        if let Some((_, signal)) = requested_signal {
            let _ = reply.send(signal);
        } else {
            self.pending_control = Some(PendingControl {
                step_id,
                report: report.clone(),
                reply,
            });
        }

        report
    }

    fn enter_waiting_control(
        &mut self,
        step: &AgentStep,
        outcome: &AgentOutcome,
    ) -> AgentTurnState {
        let state = AgentTurnState::WaitingControl {
            step: step.clone(),
            outcome: outcome.clone(),
        };
        self.state = Some(state.clone());
        state
    }

    fn take_requested_control_signal(
        &mut self,
        step_id: AgentStepId,
    ) -> Option<(AgentTurnState, AgentControlSignal)> {
        match self.deferred_control.take()? {
            DeferredControlRequest::Preempt => {
                let state = AgentTurnState::Preempting { step_id };
                self.state = Some(state.clone());
                Some((state, AgentControlSignal::preempt()))
            }
            DeferredControlRequest::Pause => {
                let state = AgentTurnState::Paused { at: step_id };
                self.state = Some(state.clone());
                let signal = self.make_pause_signal(step_id);
                Some((state, signal))
            }
        }
    }

    fn defer_pause(&mut self) {
        if self.deferred_control != Some(DeferredControlRequest::Preempt) {
            self.deferred_control = Some(DeferredControlRequest::Pause);
        }
    }

    fn defer_preempt(&mut self) {
        self.deferred_control = Some(DeferredControlRequest::Preempt);
    }

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
        if let Some(pending) = self.pending_control.take() {
            let _ = pending.reply.send(signal);
        }
    }

    fn send_pause_control(&mut self, step_id: AgentStepId) {
        let Some(pending) = self.pending_control.take() else {
            return;
        };
        let signal = self.make_pause_signal(step_id);
        if pending.reply.send(signal).is_err() {
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
                self.state = Some(AgentTurnState::Ready {
                    next: step.action.clone(),
                    step_id: step.id,
                });
            }
            AgentEvent::StepStarted { step } => {
                self.state = Some(AgentTurnState::InFlight { step: step.clone() });
            }
            AgentEvent::TurnPaused { at, .. } => {
                self.state = Some(AgentTurnState::Paused { at: *at });
            }
            AgentEvent::TurnPreempted { at, .. } => {
                self.state = Some(AgentTurnState::Preempting { step_id: *at });
            }
            AgentEvent::TurnRecovering { after, .. } => {
                self.state = Some(AgentTurnState::Recovering { after: *after });
            }
            AgentEvent::TurnFinished => {
                self.state = Some(AgentTurnState::Finished);
            }
            AgentEvent::TurnAborted => {
                self.state = Some(AgentTurnState::Aborted);
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
        AgentControlKind::FinishTurn,
        AgentControlKind::AbortTurn,
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
