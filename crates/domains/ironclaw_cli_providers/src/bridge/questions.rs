//! Plans and questions on the bridge.
//!
//! A plan update replaces the thread's plan and is broadcast; the latest one
//! outlives the turn so a later stream still shows it. A question waits like
//! an approval gate: it is recorded in `orch_cli_approvals` while open (the
//! inbox's "Needs you"), broadcast as a form, and ends with the person's
//! answer, a dismissal, the words of a steer, a timeout, or the turn's end.

use ironclaw_host_api::agent_plan::{AgentPlan, QuestionAnswer, QuestionForm, QuestionFormError};
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::cli_approvals::{CliApprovalDecision, CliApprovalRecord};
use ironclaw_orchestrator::model::now_ms;
use tokio::sync::oneshot;

use super::{CallClaim, CliBridge, LiveEvent, QuestionView};

/// How one question ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionOutcome {
    Answered(QuestionAnswer),
    /// The person dismissed the form.
    Declined,
    /// The person sent a message instead of using the form.
    Replied(String),
    TimedOut,
    /// The turn ended (or no live turn was there to ask in).
    TurnEnded,
}

impl QuestionOutcome {
    /// The answer, when there is one; every other outcome reads to the
    /// agent as a declined question, with [`Self::note`] saying why.
    pub fn answer(&self) -> Option<&QuestionAnswer> {
        match self {
            Self::Answered(answer) => Some(answer),
            _ => None,
        }
    }

    /// What the agent reads when there is no answer.
    pub fn note(&self) -> String {
        match self {
            Self::Answered(_) => String::new(),
            Self::Declined => ironclaw_host_api::agent_plan::QUESTION_DECLINED_MESSAGE.to_string(),
            Self::Replied(text) => {
                format!("The user did not use the form and wrote instead: {text}")
            }
            Self::TimedOut => "Nobody answered the question in time.".to_string(),
            Self::TurnEnded => "The turn ended before the question was answered.".to_string(),
        }
    }

    fn recorded(&self) -> CliApprovalDecision {
        match self {
            Self::Answered(_) => CliApprovalDecision::Allowed,
            Self::Declined | Self::Replied(_) => CliApprovalDecision::Declined,
            Self::TimedOut => CliApprovalDecision::TimedOut,
            Self::TurnEnded => CliApprovalDecision::TurnEnded,
        }
    }
}

pub(super) struct PendingQuestion {
    pub(super) view: QuestionView,
    pub(super) answer: oneshot::Sender<QuestionOutcome>,
}

impl CliBridge {
    /// The agent replaced its plan: keep it as the thread's plan and show
    /// it. Ignored for a run that is not live.
    pub fn plan_updated(&self, thread: &ThreadId, run_id: &str, plan: AgentPlan) {
        if !self.is_live(thread, run_id) {
            return;
        }
        let Ok(mut plans) = self.plans.lock() else {
            return; // silent-ok: a poisoned map only follows a panic elsewhere
        };
        let seq = plans.get(thread).map_or(1, |(seq, _, _)| seq + 1);
        plans.insert(thread.clone(), (seq, run_id.to_string(), plan.clone()));
        drop(plans);
        self.emit(LiveEvent::Plan {
            thread: thread.clone(),
            run_id: run_id.to_string(),
            seq,
            plan,
        });
    }

    /// The thread's latest plan: its update number, run and plan.
    pub fn plan_of(&self, thread: &ThreadId) -> Option<(u64, String, AgentPlan)> {
        self.plans.lock().ok()?.get(thread).cloned()
    }

    /// Put `form` to the thread's owner and wait for how it ends. `tool`
    /// names the asking tool in the approval record. The claim (taken where
    /// the driver read the request) places the form after what the model
    /// wrote before it.
    pub async fn ask(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool: &str,
        form: QuestionForm,
        claim: CallClaim,
    ) -> QuestionOutcome {
        if !claim.live || form.validate().is_err() {
            return QuestionOutcome::TurnEnded;
        }
        let view = QuestionView {
            gate_ref: format!(
                "{}{}",
                ironclaw_host_api::agent_plan::QUESTION_GATE_REF_PREFIX,
                uuid::Uuid::new_v4().simple()
            ),
            run_id: run_id.to_string(),
            form,
        };
        if let Some(ledger) = self.ledger.get() {
            let record = CliApprovalRecord {
                gate_ref: view.gate_ref.clone(),
                thread_id: thread.clone(),
                run_id: run_id.to_string(),
                tool_name: tool.to_string(),
                body: view.form.fields.first().map(|f| f.label.clone()),
                requested_at: now_ms(),
                decision: None,
                reason: None,
                decided_at: None,
            };
            if let Err(error) = ledger.cli_approval_opened(&record).await {
                tracing::debug!(%error, "cli bridge: question not recorded; not asked");
                return QuestionOutcome::TurnEnded;
            }
        }
        let (tx, rx) = oneshot::channel();
        let registered = self.live.lock().ok().and_then(|mut live| {
            let turn = live.get_mut(thread).filter(|t| t.run_id == run_id)?;
            turn.questions.push(PendingQuestion {
                view: view.clone(),
                answer: tx,
            });
            Some(turn.flush())
        });
        let Some(unsent) = registered else {
            self.record_close(&view.gate_ref, CliApprovalDecision::TurnEnded, None)
                .await;
            return QuestionOutcome::TurnEnded;
        };
        self.emit_text(thread, run_id, unsent);
        self.emit(LiveEvent::QuestionOpened {
            thread: thread.clone(),
            question: view.clone(),
        });
        let outcome = match tokio::time::timeout(self.approval_timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => QuestionOutcome::TurnEnded,
            Err(_) => QuestionOutcome::TimedOut,
        };
        let note = outcome.note();
        let reason = (!note.is_empty()).then_some(note.as_str());
        self.record_close(&view.gate_ref, outcome.recorded(), reason)
            .await;
        if let Ok(mut live) = self.live.lock()
            && let Some(turn) = live.get_mut(thread)
        {
            turn.questions.retain(|q| q.view.gate_ref != view.gate_ref);
        }
        self.emit(LiveEvent::QuestionClosed {
            thread: thread.clone(),
            run_id: run_id.to_string(),
            gate_ref: view.gate_ref,
        });
        outcome
    }

    /// Answer (`Some`) or dismiss (`None`) a pending question on `thread`.
    /// `Ok(false)` when there is no such question; an answer that does not
    /// fit the form is refused and the question stays open.
    pub fn answer_question(
        &self,
        thread: &ThreadId,
        gate_ref: &str,
        answer: Option<QuestionAnswer>,
    ) -> Result<bool, QuestionFormError> {
        let Ok(mut live) = self.live.lock() else {
            return Ok(false); // silent-ok: a poisoned map only follows a panic elsewhere
        };
        let Some(turn) = live.get_mut(thread) else {
            return Ok(false);
        };
        let Some(at) = turn
            .questions
            .iter()
            .position(|q| q.view.gate_ref == gate_ref)
        else {
            return Ok(false);
        };
        if let Some(answer) = &answer {
            turn.questions[at].view.form.check_answer(answer)?;
        }
        let pending = turn.questions.remove(at);
        let outcome = answer.map_or(QuestionOutcome::Declined, QuestionOutcome::Answered);
        Ok(pending.answer.send(outcome).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ironclaw_host_api::agent_plan::{
        PlanEntry, PlanEntryStatus, QuestionField, QuestionFieldKind, QuestionOption, QuestionValue,
    };

    use super::*;

    fn tid() -> ThreadId {
        ThreadId::new("thread-q").unwrap()
    }

    fn form() -> QuestionForm {
        QuestionForm {
            source: Some("Claude".into()),
            message: None,
            fields: vec![QuestionField {
                id: "q".into(),
                label: "Ship it?".into(),
                header: None,
                description: None,
                required: true,
                kind: QuestionFieldKind::Choice {
                    options: vec![QuestionOption::new("Yes"), QuestionOption::new("No")],
                    multi: false,
                    allow_other: false,
                },
            }],
        }
    }

    async fn opened(rx: &mut tokio::sync::broadcast::Receiver<LiveEvent>) -> QuestionView {
        loop {
            if let LiveEvent::QuestionOpened { question, .. } = rx.recv().await.unwrap() {
                return question;
            }
        }
    }

    #[tokio::test]
    async fn a_question_waits_for_a_fitting_answer() {
        let bridge = CliBridge::new(Duration::from_secs(5));
        bridge.turn_started(&tid(), "run-1");
        let mut rx = bridge.subscribe();
        let asker = Arc::clone(&bridge);
        let waiting = tokio::spawn(async move {
            let claim = asker.claim_call(&tid(), "run-1");
            asker
                .ask(&tid(), "run-1", "AskUserQuestion", form(), claim)
                .await
        });
        let view = opened(&mut rx).await;
        assert_eq!(
            bridge.snapshot(&tid()).unwrap().questions,
            vec![view.clone()]
        );
        let mut bad = QuestionAnswer::default();
        bad.values
            .insert("q".into(), QuestionValue::Choices(vec!["Maybe".into()]));
        assert!(
            bridge
                .answer_question(&tid(), &view.gate_ref, Some(bad))
                .is_err()
        );
        let mut good = QuestionAnswer::default();
        good.values
            .insert("q".into(), QuestionValue::Choices(vec!["Yes".into()]));
        assert_eq!(
            bridge.answer_question(&tid(), &view.gate_ref, Some(good.clone())),
            Ok(true)
        );
        assert_eq!(waiting.await.unwrap(), QuestionOutcome::Answered(good));
        assert!(bridge.snapshot(&tid()).unwrap().questions.is_empty());
        assert_eq!(
            bridge.answer_question(&tid(), &view.gate_ref, None),
            Ok(false),
            "answered once"
        );
    }

    #[tokio::test]
    async fn dismissal_steer_and_turn_end_each_end_the_question() {
        let bridge = CliBridge::new(Duration::from_secs(5));
        for (case, run) in ["dismiss", "steer", "end"].iter().zip(["r1", "r2", "r3"]) {
            bridge.turn_started(&tid(), run);
            let mut rx = bridge.subscribe();
            let asker = Arc::clone(&bridge);
            let waiting = tokio::spawn(async move {
                let claim = asker.claim_call(&tid(), run);
                asker
                    .ask(&tid(), run, "AskUserQuestion", form(), claim)
                    .await
            });
            let view = opened(&mut rx).await;
            let outcome = match *case {
                "dismiss" => {
                    assert_eq!(
                        bridge.answer_question(&tid(), &view.gate_ref, None),
                        Ok(true)
                    );
                    waiting.await.unwrap()
                }
                "steer" => {
                    assert_eq!(bridge.decline_pending(&tid(), run, "use sqlite"), 1);
                    waiting.await.unwrap()
                }
                _ => {
                    bridge.turn_ended(&tid(), run, "cancelled");
                    waiting.await.unwrap()
                }
            };
            let want = match *case {
                "dismiss" => QuestionOutcome::Declined,
                "steer" => QuestionOutcome::Replied("use sqlite".into()),
                _ => QuestionOutcome::TurnEnded,
            };
            assert_eq!(outcome, want, "{case}");
            assert!(outcome.answer().is_none());
            assert!(!outcome.note().is_empty());
        }
    }

    #[tokio::test]
    async fn a_question_without_a_live_turn_is_not_asked() {
        let bridge = CliBridge::new(Duration::from_secs(5));
        let claim = bridge.claim_call(&tid(), "gone");
        assert_eq!(
            bridge
                .ask(&tid(), "gone", "AskUserQuestion", form(), claim)
                .await,
            QuestionOutcome::TurnEnded
        );
    }

    #[tokio::test]
    async fn plans_replace_each_other_and_outlive_the_turn() {
        let bridge = CliBridge::new(Duration::from_secs(5));
        let plan =
            |status| AgentPlan::new([PlanEntry::new("Step", status)], None, chrono::Utc::now());
        bridge.plan_updated(&tid(), "r1", plan(PlanEntryStatus::Pending));
        assert!(bridge.plan_of(&tid()).is_none(), "no live turn, no plan");
        bridge.turn_started(&tid(), "r1");
        let mut rx = bridge.subscribe();
        bridge.plan_updated(&tid(), "r1", plan(PlanEntryStatus::InProgress));
        bridge.plan_updated(&tid(), "r1", plan(PlanEntryStatus::Completed));
        let mut seqs = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let LiveEvent::Plan { seq, .. } = event {
                seqs.push(seq);
            }
        }
        assert_eq!(seqs, vec![1, 2]);
        assert_eq!(bridge.snapshot(&tid()).unwrap().plan.unwrap().0, 2);
        bridge.turn_ended(&tid(), "r1", "completed");
        let (seq, run, kept) = bridge.plan_of(&tid()).unwrap();
        assert_eq!((seq, run.as_str()), (2, "r1"));
        assert!(kept.is_complete());
    }
}
