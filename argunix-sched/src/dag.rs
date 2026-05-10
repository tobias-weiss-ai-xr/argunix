//! Dependency-aware scheduling strategy.
//!
//! Builds an in-memory `StepGraph` from the head + closure of every
//! [`ScheduleItem`], deduplicating Steps by drv path so a derivation
//! shared between two Jobs (the canonical example: an internal
//! `glibc.drv` that two unrelated top-level packages both transitively
//! depend on) is realised exactly once. Dispatch is gated on
//! dependency completion: a Step is only handed to the cross-repo
//! WFQ queue when all its in-graph deps have terminated successfully.
//!
//! When a Step fails, its rdep transitive closure is BFS-walked and
//! marked `Skipped`. Head Jobs whose head Step (or any internal Step
//! in its closure) gets skipped surface in
//! [`crate::CompletionEffects::cascaded_skips`] so the daemon can mark
//! the corresponding DB rows + post forge checks without ever
//! receiving a `Dispatched` for them.
//!
//! ## Limitations (V1)
//!
//! - Steps whose deps had already failed at the moment of `enqueue`
//!   are not reactively marked `Skipped`. This matters only when one
//!   eval's failures land before a *later* eval enqueues and shares
//!   the same drv graph; the daemon avoids it today by single-eval
//!   processing.
//! - No cancel API yet. `cancel_eval(eval_id)` will land alongside the
//!   daemon-side cancel-on-push wiring.
//! - Builder affinity (preferring a builder that already has my deps'
//!   outputs) is the daemon's `pick_builder_for_step`'s job, not the
//!   strategy's. The strategy only decides *what* to dispatch; *where*
//!   is downstream.

use crate::wfq::WfqCore;
use crate::{
    AliasCompletion, CascadedSkip, CompletionEffects, DerivationInfo, DispatchToken, Dispatched,
    ScheduleItem, ScheduleStrategy,
};
use argunix_domain::{EvalId, JobId, JobStatus, RepoId};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    /// Has unfinished in-graph deps; not yet eligible to dispatch.
    Pending,
    /// All deps satisfied; in WFQ's pending queue, awaiting `dispatch()`.
    Ready,
    /// Popped from WFQ; daemon is realising it on a builder.
    Running,
    Success,
    Cached,
    Failure,
    Cancelled,
    /// A dep failed; this Step never ran and never will.
    Skipped,
}

impl StepState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            StepState::Success
                | StepState::Cached
                | StepState::Failure
                | StepState::Cancelled
                | StepState::Skipped
        )
    }

    fn is_successful_terminal(self) -> bool {
        matches!(self, StepState::Success | StepState::Cached)
    }

    fn from_status(status: JobStatus) -> Self {
        match status {
            JobStatus::Success => StepState::Success,
            JobStatus::Cached => StepState::Cached,
            JobStatus::Failure => StepState::Failure,
            JobStatus::Cancelled => StepState::Cancelled,
            JobStatus::SkippedNoBuilder => StepState::Skipped,
            // Non-terminal statuses are debug_asserted away by the
            // trait contract; map to Failure as a defensive fallback.
            JobStatus::Queued | JobStatus::Running | JobStatus::Interrupted => StepState::Failure,
        }
    }
}

#[derive(Debug)]
struct Step {
    drv_path: String,
    system: Option<String>,
    required_features: Vec<String>,
    /// Repo of the eval that introduced this Step. When two evals (in
    /// principle: future cross-eval dedup) share a Step the first
    /// inserter wins; WFQ fairness still attributes its dispatch to
    /// that repo.
    repo_id: RepoId,
    eval_id: EvalId,

    state: StepState,
    deps_unfinished: usize,
    /// drv paths that depend on me. Populated as edges are added.
    rdeps: Vec<String>,
    /// Top-level Jobs whose `head_drv == self.drv_path`. Empty for
    /// internal Steps. Multiple entries when a Nix flake exposes the
    /// same derivation under several attribute paths
    /// (`pkgs.foo` and `pkgs.bar` aliasing each other, NixOS test
    /// re-exports, …): the Step is dispatched exactly once but the
    /// terminal status is mirrored into every aliased Job's DB row
    /// + forge check via [`AliasCompletion`] entries on
    /// [`CompletionEffects`].
    head_jobs: Vec<JobId>,
    /// Set when state is `Ready` or `Running`; cleared at terminal
    /// transitions. Lets `complete` look the Step back up via token
    /// without keeping a separate map (we still keep one — see
    /// `token_to_drv` — because tokens are issued before WFQ pops).
    in_flight_token: Option<DispatchToken>,
}

#[derive(Debug)]
pub struct DagStrategy {
    wfq: WfqCore<DispatchToken>,
    by_drv: HashMap<String, Step>,
    /// token → drv_path. Populated when a Step is promoted to Ready;
    /// cleared in `complete`. Exists in addition to
    /// `Step::in_flight_token` because WFQ pops by token and we need
    /// to reach the Step from the token without scanning `by_drv`.
    token_to_drv: HashMap<DispatchToken, String>,
    next_token: u64,
    /// Job metadata for synthesising [`CascadedSkip`] entries. Populated
    /// in `enqueue`, never cleared in V1 (the strategy is per-eval-set
    /// in practice; cleanup is a daemon-level concern).
    job_meta: HashMap<JobId, (RepoId, EvalId)>,
}

impl DagStrategy {
    pub fn new(concurrency_cap: Option<usize>) -> Self {
        Self {
            wfq: WfqCore::new(concurrency_cap),
            by_drv: HashMap::new(),
            token_to_drv: HashMap::new(),
            next_token: 0,
            job_meta: HashMap::new(),
        }
    }

    fn mint_token(&mut self) -> DispatchToken {
        let t = DispatchToken(self.next_token);
        self.next_token += 1;
        t
    }

    /// Insert a Step for `drv` if absent. Returns true iff this call
    /// inserted a new entry (caller uses that to decide whether to
    /// build edges for it). Existing entries are left untouched —
    /// metadata fields (system, required_features) come from the first
    /// inserter, which is acceptable because the same drv path is the
    /// same derivation, definitionally.
    fn upsert_step(&mut self, drv: &DerivationInfo, repo_id: RepoId, eval_id: EvalId) -> bool {
        if self.by_drv.contains_key(&drv.drv_path) {
            return false;
        }
        self.by_drv.insert(
            drv.drv_path.clone(),
            Step {
                drv_path: drv.drv_path.clone(),
                system: drv.system.clone(),
                required_features: drv.required_features.clone(),
                repo_id,
                eval_id,
                state: StepState::Pending,
                deps_unfinished: 0,
                rdeps: Vec::new(),
                head_jobs: Vec::new(),
                in_flight_token: None,
            },
        );
        true
    }

    /// For each newly-inserted Step, walk its declared `input_drvs` and
    /// add edges to in-graph inputs. Inputs not in `by_drv` are external
    /// (substituted at build time by Nix) and skipped — exactly the
    /// behaviour we want for transitive nixpkgs deps the eval didn't
    /// surface as Steps.
    fn build_edges_for(
        &mut self,
        newly_inserted: &[String],
        inputs_by_drv: &HashMap<String, Vec<String>>,
    ) {
        for drv_path in newly_inserted {
            let inputs = match inputs_by_drv.get(drv_path) {
                Some(v) => v,
                None => continue,
            };
            for input in inputs {
                let dep_state = match self.by_drv.get(input) {
                    Some(s) => s.state,
                    None => continue, // external dep; not gated on
                };
                if dep_state.is_successful_terminal() {
                    // Dep already done by the time we registered this
                    // Step; no gating needed.
                    continue;
                }
                // Add the edge: this Step waits on input; input gains
                // this Step as an rdep.
                self.by_drv.get_mut(drv_path).unwrap().deps_unfinished += 1;
                self.by_drv
                    .get_mut(input)
                    .unwrap()
                    .rdeps
                    .push(drv_path.clone());
            }
        }
    }

    /// Promote any Pending Step whose deps are satisfied to Ready, mint
    /// a token, and push to WFQ. Idempotent: Steps already in Ready /
    /// Running / terminal are skipped.
    fn promote_ready_steps(&mut self) {
        let to_promote: Vec<(String, RepoId)> = self
            .by_drv
            .iter()
            .filter_map(|(drv_path, step)| {
                if matches!(step.state, StepState::Pending) && step.deps_unfinished == 0 {
                    Some((drv_path.clone(), step.repo_id))
                } else {
                    None
                }
            })
            .collect();
        for (drv_path, repo_id) in to_promote {
            let token = self.mint_token();
            {
                let step = self.by_drv.get_mut(&drv_path).unwrap();
                step.state = StepState::Ready;
                step.in_flight_token = Some(token);
            }
            self.token_to_drv.insert(token, drv_path);
            self.wfq.enqueue(repo_id, token);
        }
    }

    /// BFS the rdep closure of `from_drv`, marking each non-terminal
    /// Step as Skipped. Returns one [`CascadedSkip`] per head Job that
    /// got skipped along the way so the caller can surface them on
    /// `CompletionEffects`. A Step with multiple aliased head Jobs
    /// emits one entry per alias.
    fn cascade_skip(&mut self, from_drv: &str) -> Vec<CascadedSkip> {
        let mut out = Vec::new();
        let initial_rdeps = self
            .by_drv
            .get(from_drv)
            .map(|s| s.rdeps.clone())
            .unwrap_or_default();
        let mut queue: VecDeque<String> = VecDeque::from(initial_rdeps);
        while let Some(rdep_path) = queue.pop_front() {
            let (head_jobs, further_rdeps) = {
                let rdep = match self.by_drv.get_mut(&rdep_path) {
                    Some(s) => s,
                    None => continue,
                };
                if rdep.state.is_terminal() {
                    continue;
                }
                rdep.state = StepState::Skipped;
                // If this Step had been promoted to Ready, drop the
                // dangling token mapping so a stray complete() does
                // nothing. Note: WFQ still has the tag in its pending
                // queue; we leave it to be popped and ignored. (See
                // `dispatch` for the swallow.)
                if let Some(tok) = rdep.in_flight_token.take() {
                    self.token_to_drv.remove(&tok);
                }
                (rdep.head_jobs.clone(), rdep.rdeps.clone())
            };
            for job_id in head_jobs {
                if let Some(&(repo_id, eval_id)) = self.job_meta.get(&job_id) {
                    out.push(CascadedSkip {
                        job_id,
                        eval_id,
                        repo_id,
                        reason_drv: from_drv.to_string(),
                    });
                }
            }
            queue.extend(further_rdeps);
        }
        out
    }
}

impl Default for DagStrategy {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ScheduleStrategy for DagStrategy {
    fn set_weight(&mut self, repo_id: RepoId, weight: u32) {
        self.wfq.set_weight(repo_id, weight);
    }

    fn enqueue(&mut self, item: ScheduleItem) {
        self.job_meta
            .insert(item.job_id, (item.repo_id, item.eval_id));

        // Collect input_drvs for every drv we'll consider in this
        // enqueue, before mutating by_drv (so we don't double-borrow).
        let mut inputs_by_drv: HashMap<String, Vec<String>> = HashMap::new();
        inputs_by_drv.insert(
            item.head_drv.drv_path.clone(),
            item.head_drv.input_drvs.clone(),
        );
        for drv in &item.closure {
            inputs_by_drv.insert(drv.drv_path.clone(), drv.input_drvs.clone());
        }

        // Insert Steps; remember which ones are new so we know whose
        // edges to build. Closure first so head_drv's edges resolve to
        // already-present Step entries.
        let mut newly_inserted: Vec<String> = Vec::new();
        for drv in &item.closure {
            if self.upsert_step(drv, item.repo_id, item.eval_id) {
                newly_inserted.push(drv.drv_path.clone());
            }
        }
        let head_was_new = self.upsert_step(&item.head_drv, item.repo_id, item.eval_id);
        if head_was_new {
            newly_inserted.push(item.head_drv.drv_path.clone());
        }

        // Wire the head Step to the Job. Multiple Jobs can share a
        // head drv (Nix flakes routinely re-export the same derivation
        // under several attribute paths — `pkgs.foo` and `pkgs.bar`,
        // NixOS-test re-exports, …). We dispatch the Step exactly
        // once and surface the terminal status to every aliased Job
        // via [`AliasCompletion`] entries on `CompletionEffects`.
        let head = self.by_drv.get_mut(&item.head_drv.drv_path).unwrap();
        head.head_jobs.push(item.job_id);

        self.build_edges_for(&newly_inserted, &inputs_by_drv);
        self.promote_ready_steps();
    }

    fn dispatch(&mut self) -> Option<Dispatched> {
        // WFQ may hand back a token whose Step was cascade-skipped
        // after the token was enqueued; in that case `token_to_drv`
        // has been cleared, and we silently swallow the stale tag and
        // try again.
        loop {
            let popped = self.wfq.dispatch()?;
            let drv_path = match self.token_to_drv.get(&popped.tag) {
                Some(p) => p.clone(),
                None => {
                    // Stale Ready entry from a cascade-skip. Free its
                    // in-flight slot and try the next.
                    self.wfq.complete(popped.tag);
                    continue;
                }
            };
            let step = self.by_drv.get_mut(&drv_path).unwrap();
            // Sanity: state should be Ready when WFQ pops it.
            debug_assert_eq!(step.state, StepState::Ready);
            step.state = StepState::Running;
            return Some(Dispatched {
                token: popped.tag,
                repo_id: popped.repo_id,
                eval_id: step.eval_id,
                drv_path: step.drv_path.clone(),
                system: step.system.clone(),
                required_features: step.required_features.clone(),
                // Pick the first head Job as the primary; aliases (if
                // any) surface on `complete` via `alias_completions`.
                head_job: step.head_jobs.first().copied(),
            });
        }
    }

    fn complete(&mut self, token: DispatchToken, status: JobStatus) -> CompletionEffects {
        debug_assert!(
            status.is_terminal(),
            "complete called with non-terminal status: {status:?}",
        );

        let Some(drv_path) = self.token_to_drv.remove(&token) else {
            // Token unknown: either the dispatch was synthesised by a
            // racy double-complete or we cascade-skipped the Step
            // before its token came back.
            return CompletionEffects::default();
        };

        let new_state = StepState::from_status(status);
        let (rdeps, succeeded, head_jobs) = {
            let step = self.by_drv.get_mut(&drv_path).unwrap();
            step.state = new_state;
            step.in_flight_token = None;
            (
                step.rdeps.clone(),
                new_state.is_successful_terminal(),
                step.head_jobs.clone(),
            )
        };
        let repo_id = self.wfq.complete(token);

        let mut cascaded_skips = Vec::new();

        if succeeded {
            // Decrement rdeps' deps_unfinished; anyone who hits 0 will
            // be promoted to Ready below.
            for rdep_path in &rdeps {
                if let Some(rdep) = self.by_drv.get_mut(rdep_path) {
                    rdep.deps_unfinished = rdep.deps_unfinished.saturating_sub(1);
                }
            }
            self.promote_ready_steps();
        } else {
            // Failure / Cancelled / Skipped: walk forward and skip
            // every still-Pending/Ready Step that depended on this one.
            cascaded_skips = self.cascade_skip(&drv_path);
        }

        // Aliases: every head Job past the primary mirrors the
        // primary's terminal status via the daemon's alias-completion
        // path. The primary's `head_job` is `head_jobs[0]` (matches
        // what `dispatch` returned); the rest go in alias_completions.
        let alias_completions: Vec<AliasCompletion> = head_jobs
            .iter()
            .skip(1)
            .filter_map(|jid| {
                self.job_meta
                    .get(jid)
                    .map(|&(repo_id, eval_id)| AliasCompletion {
                        job_id: *jid,
                        eval_id,
                        repo_id,
                    })
            })
            .collect();

        CompletionEffects {
            repo_id,
            cascaded_skips,
            alias_completions,
        }
    }

    fn cancel_eval(&mut self, eval_id: EvalId) -> Vec<CascadedSkip> {
        // For each Job in this eval, find its head Step. Skip it (as
        // in: mark Cancelled and cascade) only if it hasn't been
        // dispatched yet. Steps that are already Running keep going —
        // the per-eval CancelToken signals their build to abort, and
        // the resulting Cancelled status flows back through complete().
        // Internal Steps shared with another live eval also keep
        // running; cascade only walks rdeps of *this* head, so a
        // shared internal stays Pending/Ready/Running for whoever else
        // wanted it.
        let job_ids: Vec<JobId> = self
            .job_meta
            .iter()
            .filter(|(_, (_, e))| *e == eval_id)
            .map(|(jid, _)| *jid)
            .collect();

        let mut skips: Vec<CascadedSkip> = Vec::new();
        for jid in job_ids {
            // Find the head Step for this Job. by_drv is the source of
            // truth — head_jobs link there. (Linear scan; eval sizes
            // are bounded and this only fires on cancel.)
            let head_drv = self
                .by_drv
                .iter()
                .find(|(_, s)| s.head_jobs.contains(&jid))
                .map(|(d, _)| d.clone());
            let Some(head_drv) = head_drv else {
                // Already cleaned up (e.g. the Step terminated and was
                // forgotten; we forget head_job links lazily). Drop
                // the metadata and emit a skip so the daemon can
                // close out the DB row.
                let (repo_id, eval_id) = match self.job_meta.remove(&jid) {
                    Some(v) => v,
                    None => continue,
                };
                skips.push(CascadedSkip {
                    job_id: jid,
                    eval_id,
                    repo_id,
                    reason_drv: format!("eval {} cancelled", eval_id.get()),
                });
                continue;
            };

            let (was_pending_or_ready, repo_id) = {
                let step = self.by_drv.get_mut(&head_drv).unwrap();
                let pending_or_ready = matches!(step.state, StepState::Pending | StepState::Ready);
                (pending_or_ready, step.repo_id)
            };

            if was_pending_or_ready {
                let cancelled_jobs: Vec<JobId> = {
                    let step = self.by_drv.get_mut(&head_drv).unwrap();
                    step.state = StepState::Cancelled;
                    if let Some(tok) = step.in_flight_token.take() {
                        // Drop the token mapping; if WFQ later pops it,
                        // dispatch's stale-token swallow ignores it.
                        self.token_to_drv.remove(&tok);
                    }
                    // Drop the head_jobs links so the cascade walk
                    // doesn't re-emit a skip for them; we emit direct
                    // skips ourselves below. With aliases, this Step
                    // covers multiple Jobs (`pkgs.foo` + `pkgs.bar`
                    // pointing to the same drv) — every one of them
                    // gets a skip.
                    std::mem::take(&mut step.head_jobs)
                };

                let reason = format!("eval {} cancelled", eval_id.get());
                for cjid in cancelled_jobs {
                    let (cjid_repo, cjid_eval) = self
                        .job_meta
                        .get(&cjid)
                        .copied()
                        .unwrap_or((repo_id, eval_id));
                    skips.push(CascadedSkip {
                        job_id: cjid,
                        eval_id: cjid_eval,
                        repo_id: cjid_repo,
                        reason_drv: reason.clone(),
                    });
                }

                // Cascade through rdeps. cascade_skip's own emitted
                // skips carry reason_drv = head_drv; rewrite to the
                // eval-cancelled message so the operator-facing log
                // is uniform.
                let cascade = self.cascade_skip(&head_drv);
                let reason = format!("eval {} cancelled", eval_id.get());
                for mut s in cascade {
                    s.reason_drv = reason.clone();
                    skips.push(s);
                }
            }
            // Either way, the Job is no longer being tracked by the
            // strategy (the head Step persists for its outputs but its
            // head_job link is gone, or the Step was already Running
            // and will route through complete()). Drop the metadata
            // so cancel_eval is idempotent.
            self.job_meta.remove(&jid);
        }

        skips
    }

    fn pending_count(&self) -> usize {
        self.wfq.pending_count()
    }

    fn in_flight_count(&self) -> usize {
        self.wfq.in_flight_count()
    }

    fn pending_for(&self, repo_id: RepoId) -> usize {
        self.wfq.pending_for(repo_id)
    }
}
