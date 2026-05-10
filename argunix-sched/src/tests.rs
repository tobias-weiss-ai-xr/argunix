use super::*;
use argunix_domain::{EvalId, JobStatus};
use std::time::Duration;

fn jid(n: i64) -> JobId {
    JobId::new(n)
}

fn rid(n: i64) -> RepoId {
    RepoId::new(n)
}

/// FlatStrategy ignores everything beyond repo_id + job_id + the head
/// drv. These tests target FlatStrategy via the trait surface, so the
/// closure is always empty and head_drv carries placeholder fields
/// just so the struct can be constructed.
fn item(repo: RepoId, job: JobId) -> ScheduleItem {
    ScheduleItem {
        repo_id: repo,
        eval_id: EvalId::new(0),
        job_id: job,
        head_drv: DerivationInfo {
            drv_path: format!("/nix/store/{}-job{}.drv", "x".repeat(32), job.get()),
            system: None,
            required_features: Vec::new(),
            input_drvs: Vec::new(),
        },
        closure: Vec::new(),
    }
}

/// Pop one Dispatched from the strategy and immediately complete it
/// with `status`. Returns the dispatch's token + repo so callers can
/// reason about ordering. Panics if the strategy returned `None`.
fn dispatch_and_complete(s: &mut dyn ScheduleStrategy, status: JobStatus) -> Dispatched {
    let d = s.dispatch().expect("expected a dispatch");
    let token = d.token;
    let _ = s.complete(token, status);
    d
}

#[test]
fn empty_scheduler_dispatches_nothing() {
    let mut s = FlatStrategy::new(None);
    assert!(s.dispatch().is_none());
}

#[test]
fn single_repo_dispatches_in_fifo_order() {
    let mut s = FlatStrategy::new(None);
    s.enqueue(item(rid(1), jid(10)));
    s.enqueue(item(rid(1), jid(11)));
    s.enqueue(item(rid(1), jid(12)));

    assert_eq!(s.dispatch().unwrap().head_job, Some(jid(10)));
    assert_eq!(s.dispatch().unwrap().head_job, Some(jid(11)));
    assert_eq!(s.dispatch().unwrap().head_job, Some(jid(12)));
    assert!(s.dispatch().is_none());
}

#[test]
fn two_equal_weight_repos_alternate() {
    let mut s = FlatStrategy::new(None);
    s.enqueue(item(rid(1), jid(10)));
    s.enqueue(item(rid(1), jid(11)));
    s.enqueue(item(rid(2), jid(20)));
    s.enqueue(item(rid(2), jid(21)));

    // Both at vt=0; tie broken by earliest enqueue seq → repo 1 first.
    let d1 = dispatch_and_complete(&mut s, JobStatus::Success);
    assert_eq!(d1.repo_id, rid(1));

    // Repo 1's vt is now 1.0; repo 2 still at 0 → repo 2 wins.
    let d2 = dispatch_and_complete(&mut s, JobStatus::Success);
    assert_eq!(d2.repo_id, rid(2));

    // Both back at vt=1.0; tie broken by remaining earliest seq → repo 1.
    let d3 = dispatch_and_complete(&mut s, JobStatus::Success);
    assert_eq!(d3.repo_id, rid(1));

    let d4 = s.dispatch().unwrap();
    assert_eq!(d4.repo_id, rid(2));
}

#[test]
fn weight_2_to_1_dispatches_at_2_to_1_ratio() {
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 2);
    s.set_weight(rid(2), 1);
    for n in 0..1000 {
        s.enqueue(item(rid(1), jid(n)));
        s.enqueue(item(rid(2), jid(1000 + n)));
    }

    let mut a = 0;
    let mut b = 0;
    for _ in 0..600 {
        if let Some(d) = s.dispatch() {
            if d.repo_id == rid(1) {
                a += 1;
            } else {
                b += 1;
            }
            s.complete(d.token, JobStatus::Success);
        }
    }

    let ratio = a as f64 / b as f64;
    assert!(
        ratio > 1.9 && ratio < 2.1,
        "expected ratio ~2:1, got {a}:{b} = {ratio}",
    );
}

#[test]
fn weight_5_to_1_clearly_favours_heavy_repo() {
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 5);
    s.set_weight(rid(2), 1);
    for n in 0..2000 {
        s.enqueue(item(rid(1), jid(n)));
        s.enqueue(item(rid(2), jid(2000 + n)));
    }

    let mut a = 0;
    let mut b = 0;
    for _ in 0..1200 {
        if let Some(d) = s.dispatch() {
            if d.repo_id == rid(1) {
                a += 1;
            } else {
                b += 1;
            }
            s.complete(d.token, JobStatus::Success);
        }
    }
    let ratio = a as f64 / b as f64;
    assert!(ratio > 4.7 && ratio < 5.3, "ratio = {ratio} ({a}:{b})");
}

#[test]
fn concurrency_cap_blocks_dispatch_until_complete() {
    let mut s = FlatStrategy::new(Some(2));
    s.enqueue(item(rid(1), jid(1)));
    s.enqueue(item(rid(1), jid(2)));
    s.enqueue(item(rid(1), jid(3)));

    let d1 = s.dispatch().unwrap();
    let d2 = s.dispatch().unwrap();
    assert_eq!(s.in_flight_count(), 2);
    assert!(
        s.dispatch().is_none(),
        "third dispatch should be blocked by cap"
    );

    s.complete(d1.token, JobStatus::Success);
    let d3 = s.dispatch().unwrap();
    assert_eq!(d3.head_job, Some(jid(3)));
    s.complete(d2.token, JobStatus::Success);
    s.complete(d3.token, JobStatus::Success);
    assert!(s.dispatch().is_none());
    assert_eq!(s.in_flight_count(), 0);
}

#[test]
fn complete_returns_repo_for_known_jobs_and_none_otherwise() {
    let mut s = FlatStrategy::new(None);
    s.enqueue(item(rid(7), jid(100)));
    let d = s.dispatch().unwrap();
    assert_eq!(
        s.complete(d.token, JobStatus::Success).repo_id,
        Some(rid(7))
    );
    // Same token completed twice: second complete is a no-op (repo None).
    assert_eq!(s.complete(d.token, JobStatus::Success).repo_id, None);
}

#[test]
fn idle_repo_does_not_advance_in_virtual_time() {
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 1);
    s.set_weight(rid(2), 1);
    s.enqueue(item(rid(2), jid(1)));
    let _ = s.dispatch();
    assert_eq!(s.virtual_time_for(rid(1)).unwrap(), 0.0);
    assert!(s.virtual_time_for(rid(2)).unwrap() > 0.0);
}

#[test]
fn long_idle_repo_does_not_get_unbounded_advantage() {
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 1);
    s.set_weight(rid(2), 1);
    for n in 0..50 {
        s.enqueue(item(rid(1), jid(n)));
    }
    for _ in 0..30 {
        dispatch_and_complete(&mut s, JobStatus::Success);
    }
    // Repo 1's vt is now 30. Repo 2 arrives.
    s.enqueue(item(rid(2), jid(100)));
    let snapped_vt = s.virtual_time_for(rid(2)).unwrap();
    assert_eq!(snapped_vt, 30.0);
}

#[test]
fn tick_is_a_noop() {
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 5);
    s.enqueue(item(rid(1), jid(1)));
    let before = s.virtual_time_for(rid(1)).unwrap();
    s.tick(Duration::from_secs(60));
    s.tick(Duration::ZERO);
    assert_eq!(s.virtual_time_for(rid(1)).unwrap(), before);
}

#[test]
fn many_repos_random_arrivals_no_starvation() {
    let mut s = FlatStrategy::new(Some(3));
    for i in 1..=5 {
        s.set_weight(rid(i), (i as u32 - 1).max(1));
    }
    for round in 0..500 {
        for repo in 1..=5_i64 {
            s.enqueue(item(rid(repo), jid(round * 5 + repo)));
        }
    }

    let mut iterations = 0;
    while s.pending_count() > 0 || s.in_flight_count() > 0 {
        if let Some(d) = s.dispatch() {
            s.complete(d.token, JobStatus::Success);
        }
        iterations += 1;
        assert!(
            iterations < 10_000,
            "scheduler stuck: {} pending, {} in-flight",
            s.pending_count(),
            s.in_flight_count(),
        );
    }
    for i in 1..=5 {
        assert_eq!(s.pending_for(rid(i)), 0);
    }
}

#[test]
fn ties_broken_by_earliest_enqueue() {
    let mut s = FlatStrategy::new(None);
    s.enqueue(item(rid(2), jid(20)));
    s.enqueue(item(rid(1), jid(10)));
    let d = s.dispatch().unwrap();
    assert_eq!(d.repo_id, rid(2));
}

#[test]
fn auto_registers_unseen_repo_with_default_weight() {
    let mut s = FlatStrategy::new(None);
    s.enqueue(item(rid(42), jid(1)));
    assert_eq!(s.pending_for(rid(42)), 1);
    let d = s.dispatch().unwrap();
    assert_eq!(d.repo_id, rid(42));
}

#[test]
fn newly_arriving_repo_is_not_starved_by_a_busy_high_weight_one() {
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 10);
    s.set_weight(rid(2), 1);
    for n in 0..200 {
        s.enqueue(item(rid(1), jid(n)));
    }
    for _ in 0..50 {
        dispatch_and_complete(&mut s, JobStatus::Success);
    }
    s.enqueue(item(rid(2), jid(1000)));
    s.enqueue(item(rid(2), jid(1001)));
    s.enqueue(item(rid(2), jid(1002)));

    let mut got_repo_2 = 0;
    for _ in 0..100 {
        if let Some(d) = s.dispatch() {
            if d.repo_id == rid(2) {
                got_repo_2 += 1;
            }
            s.complete(d.token, JobStatus::Success);
        }
    }
    assert!(
        got_repo_2 >= 3,
        "repo 2 should have drained its 3 pending jobs; got {got_repo_2}",
    );
}

#[test]
fn build_factory_returns_a_working_strategy() {
    // The factory's job is to wire kind → impl. Smoke-test that the
    // dispatch loop works end-to-end through `Box<dyn ScheduleStrategy>`,
    // since that's how the daemon will hold it.
    let mut s = build(SchedulerKind::Flat, Some(1));
    s.enqueue(item(rid(1), jid(1)));
    s.enqueue(item(rid(1), jid(2)));
    let d = s.dispatch().unwrap();
    assert_eq!(d.head_job, Some(jid(1)));
    assert!(s.dispatch().is_none(), "cap should block second dispatch");
    s.complete(d.token, JobStatus::Success);
    let d2 = s.dispatch().unwrap();
    assert_eq!(d2.head_job, Some(jid(2)));
}

#[test]
fn build_factory_constructs_dag_too() {
    // Smoke-test that DagStrategy is wired into the factory and behaves
    // like FlatStrategy when items have no closure (degenerate DAG).
    let mut s = build(SchedulerKind::Dag, None);
    s.enqueue(item(rid(1), jid(1)));
    let d = s.dispatch().unwrap();
    assert_eq!(d.head_job, Some(jid(1)));
    let eff = s.complete(d.token, JobStatus::Success);
    assert_eq!(eff.repo_id, Some(rid(1)));
    assert!(eff.cascaded_skips.is_empty());
    assert!(s.dispatch().is_none());
}

#[test]
fn dispatched_carries_head_drv_metadata_through() {
    // FlatStrategy reads the head drv from the item and surfaces it on
    // Dispatched so the daemon can hand it to nix-store --realise
    // without re-querying the DB.
    let mut s = FlatStrategy::new(None);
    s.enqueue(ScheduleItem {
        repo_id: rid(1),
        eval_id: EvalId::new(99),
        job_id: jid(1),
        head_drv: DerivationInfo {
            drv_path: "/nix/store/aaaa-hello.drv".into(),
            system: Some("x86_64-linux".into()),
            required_features: vec!["kvm".into()],
            input_drvs: Vec::new(),
        },
        closure: Vec::new(),
    });
    let d = s.dispatch().unwrap();
    assert_eq!(d.eval_id, EvalId::new(99));
    assert_eq!(d.drv_path, "/nix/store/aaaa-hello.drv");
    assert_eq!(d.system.as_deref(), Some("x86_64-linux"));
    assert_eq!(d.required_features, vec!["kvm".to_string()]);
    assert_eq!(d.head_job, Some(jid(1)));
}

// ---------------------------------------------------------------------
// DagStrategy tests
// ---------------------------------------------------------------------

/// Build a DerivationInfo with the given drv path and inputs. System +
/// required_features are placeholders; the dispatch tests don't rely on
/// them beyond checking they pass through.
fn drv(path: &str, inputs: &[&str]) -> DerivationInfo {
    DerivationInfo {
        drv_path: path.into(),
        system: Some("x86_64-linux".into()),
        required_features: Vec::new(),
        input_drvs: inputs.iter().map(|s| s.to_string()).collect(),
    }
}

fn dag_item(
    repo: RepoId,
    eval: EvalId,
    job: JobId,
    head: DerivationInfo,
    closure: Vec<DerivationInfo>,
) -> ScheduleItem {
    ScheduleItem {
        repo_id: repo,
        eval_id: eval,
        job_id: job,
        head_drv: head,
        closure,
    }
}

#[test]
fn dag_linear_chain_b_waits_for_a() {
    // The exact case the user opened with: B's drv has A's drv as an
    // input. Both are top-level Jobs in the same eval. Without DAG
    // gating, both would dispatch in parallel and B's builder would
    // rebuild A. With DAG gating, A dispatches first and B only
    // becomes Ready after A succeeds.
    let mut s = DagStrategy::new(None);
    let a_drv = drv("/nix/store/aaaa-a.drv", &[]);
    let b_drv = drv("/nix/store/bbbb-b.drv", &["/nix/store/aaaa-a.drv"]);

    // Enqueue B with its closure (just A); enqueue A as a top-level too.
    s.enqueue(dag_item(
        rid(1),
        EvalId::new(1),
        jid(1),
        a_drv.clone(),
        vec![],
    ));
    s.enqueue(dag_item(
        rid(1),
        EvalId::new(1),
        jid(2),
        b_drv,
        vec![a_drv.clone()],
    ));

    // First dispatch must be A — B is gated.
    let d_a = s.dispatch().unwrap();
    assert_eq!(d_a.drv_path, "/nix/store/aaaa-a.drv");
    assert_eq!(d_a.head_job, Some(jid(1)));
    assert!(
        s.dispatch().is_none(),
        "B must not dispatch until A succeeds",
    );

    // Complete A: B becomes Ready.
    let _ = s.complete(d_a.token, JobStatus::Success);
    let d_b = s.dispatch().unwrap();
    assert_eq!(d_b.drv_path, "/nix/store/bbbb-b.drv");
    assert_eq!(d_b.head_job, Some(jid(2)));
}

#[test]
fn dag_diamond_c_waits_for_both_a_and_b() {
    // Diamond: A and B are independent; C depends on both. C dispatches
    // only after both A and B are done, in either order.
    let mut s = DagStrategy::new(None);
    let a_drv = drv("/nix/store/aaaa-a.drv", &[]);
    let b_drv = drv("/nix/store/bbbb-b.drv", &[]);
    let c_drv = drv(
        "/nix/store/cccc-c.drv",
        &["/nix/store/aaaa-a.drv", "/nix/store/bbbb-b.drv"],
    );

    s.enqueue(dag_item(
        rid(1),
        EvalId::new(1),
        jid(1),
        a_drv.clone(),
        vec![],
    ));
    s.enqueue(dag_item(
        rid(1),
        EvalId::new(1),
        jid(2),
        b_drv.clone(),
        vec![],
    ));
    s.enqueue(dag_item(
        rid(1),
        EvalId::new(1),
        jid(3),
        c_drv,
        vec![a_drv, b_drv],
    ));

    // Pop A and B in some order; C must not appear yet.
    let d1 = s.dispatch().unwrap();
    let d2 = s.dispatch().unwrap();
    assert!(
        s.dispatch().is_none(),
        "C must wait until both A and B succeed",
    );
    let popped: Vec<&str> = vec![&d1.drv_path, &d2.drv_path];
    assert!(popped.contains(&"/nix/store/aaaa-a.drv"));
    assert!(popped.contains(&"/nix/store/bbbb-b.drv"));

    // Complete A only — C still gated by B.
    let _ = s.complete(d1.token, JobStatus::Success);
    assert!(s.dispatch().is_none(), "C still gated on the other dep");

    // Complete B — now C becomes Ready.
    let _ = s.complete(d2.token, JobStatus::Success);
    let d_c = s.dispatch().unwrap();
    assert_eq!(d_c.drv_path, "/nix/store/cccc-c.drv");
}

#[test]
fn dag_shared_internal_step_dispatched_once() {
    // The motivating problem: top-level Jobs X and Y both depend on an
    // internal Z that is NOT a top-level Job. Without dedup, X's builder
    // and Y's builder would both rebuild Z. With DAG dedup, Z is one
    // Step, dispatched exactly once with head_job = None, and BOTH X
    // and Y wait on it.
    let mut s = DagStrategy::new(None);
    let z = drv("/nix/store/zzzz-z.drv", &[]);
    let x = drv("/nix/store/xxxx-x.drv", &["/nix/store/zzzz-z.drv"]);
    let y = drv("/nix/store/yyyy-y.drv", &["/nix/store/zzzz-z.drv"]);

    // X is enqueued with Z in its closure; Y is enqueued with Z in its
    // closure. Z must dedup — same drv path, both items list it.
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), x, vec![z.clone()]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(2), y, vec![z]));

    // First dispatch is Z (the only Ready Step). It's an internal
    // Step — head_job is None.
    let d_z = s.dispatch().unwrap();
    assert_eq!(d_z.drv_path, "/nix/store/zzzz-z.drv");
    assert_eq!(d_z.head_job, None);
    assert!(s.dispatch().is_none(), "X and Y both gated on Z");

    // Complete Z once: BOTH X and Y unblock.
    let _ = s.complete(d_z.token, JobStatus::Success);
    let d1 = s.dispatch().unwrap();
    let d2 = s.dispatch().unwrap();
    assert!(s.dispatch().is_none(), "no more Steps");
    let popped_heads: Vec<Option<JobId>> = vec![d1.head_job, d2.head_job];
    assert!(popped_heads.contains(&Some(jid(1))));
    assert!(popped_heads.contains(&Some(jid(2))));
}

#[test]
fn dag_cascade_skip_on_dependency_failure() {
    // A → B → C. A fails. B and C never dispatch and surface as
    // CascadedSkips on the CompletionEffects from A's complete.
    let mut s = DagStrategy::new(None);
    let a = drv("/nix/store/aaaa-a.drv", &[]);
    let b = drv("/nix/store/bbbb-b.drv", &["/nix/store/aaaa-a.drv"]);
    let c = drv("/nix/store/cccc-c.drv", &["/nix/store/bbbb-b.drv"]);

    s.enqueue(dag_item(rid(1), EvalId::new(7), jid(1), a.clone(), vec![]));
    s.enqueue(dag_item(
        rid(1),
        EvalId::new(7),
        jid(2),
        b.clone(),
        vec![a.clone()],
    ));
    s.enqueue(dag_item(rid(1), EvalId::new(7), jid(3), c, vec![a, b]));

    let d_a = s.dispatch().unwrap();
    assert_eq!(d_a.head_job, Some(jid(1)));
    assert!(s.dispatch().is_none(), "B and C gated");

    // A fails. B and C cascade to Skipped without ever dispatching.
    let eff = s.complete(d_a.token, JobStatus::Failure);
    assert_eq!(eff.cascaded_skips.len(), 2, "B and C must cascade");
    let skipped_jobs: Vec<JobId> = eff.cascaded_skips.iter().map(|c| c.job_id).collect();
    assert!(skipped_jobs.contains(&jid(2)));
    assert!(skipped_jobs.contains(&jid(3)));
    for skip in &eff.cascaded_skips {
        assert_eq!(skip.eval_id, EvalId::new(7));
        assert_eq!(skip.repo_id, rid(1));
        assert_eq!(skip.reason_drv, "/nix/store/aaaa-a.drv");
    }
    // No dispatch should ever come for B or C.
    assert!(s.dispatch().is_none());
}

#[test]
fn dag_partial_cascade_skip_does_not_affect_unrelated_branches() {
    // A → B (fails); X → Y (independent). A's failure must not affect Y.
    let mut s = DagStrategy::new(None);
    let a = drv("/nix/store/aaaa-a.drv", &[]);
    let b = drv("/nix/store/bbbb-b.drv", &["/nix/store/aaaa-a.drv"]);
    let x = drv("/nix/store/xxxx-x.drv", &[]);
    let y = drv("/nix/store/yyyy-y.drv", &["/nix/store/xxxx-x.drv"]);

    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), a.clone(), vec![]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(2), b, vec![a]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(3), x.clone(), vec![]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(4), y, vec![x]));

    // Drain the two ready Steps (A and X) in some order.
    let d1 = s.dispatch().unwrap();
    let d2 = s.dispatch().unwrap();
    assert!(s.dispatch().is_none());
    let (a_dispatch, x_dispatch) = if d1.drv_path.contains("aaaa") {
        (d1, d2)
    } else {
        (d2, d1)
    };
    assert!(a_dispatch.drv_path.contains("aaaa"));
    assert!(x_dispatch.drv_path.contains("xxxx"));

    // X succeeds: Y becomes ready.
    let eff_x = s.complete(x_dispatch.token, JobStatus::Success);
    assert!(eff_x.cascaded_skips.is_empty());
    let d_y = s.dispatch().unwrap();
    assert_eq!(d_y.head_job, Some(jid(4)));

    // A fails: only B cascades; Y is in flight and unaffected.
    let eff_a = s.complete(a_dispatch.token, JobStatus::Failure);
    let skipped: Vec<JobId> = eff_a.cascaded_skips.iter().map(|c| c.job_id).collect();
    assert_eq!(skipped, vec![jid(2)]);
}

#[test]
fn dag_external_inputs_do_not_gate() {
    // The drv lists input drvs that are NOT in any ScheduleItem (think:
    // bash, stdenv from nixpkgs that the eval didn't surface as Jobs).
    // The Step should be Ready immediately — external inputs are
    // assumed available via substituters.
    let mut s = DagStrategy::new(None);
    let a = drv(
        "/nix/store/aaaa-a.drv",
        &[
            "/nix/store/9999-bash.drv",   // external
            "/nix/store/8888-stdenv.drv", // external
        ],
    );

    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), a, vec![]));
    let d = s.dispatch().unwrap();
    assert_eq!(d.drv_path, "/nix/store/aaaa-a.drv");
}

#[test]
fn dag_cross_repo_wfq_fairness_on_ready_set() {
    // Two unrelated drvs, one per repo, both immediately Ready. WFQ
    // should still honour cross-repo fairness on the dispatch order.
    let mut s = DagStrategy::new(None);
    s.set_weight(rid(1), 1);
    s.set_weight(rid(2), 1);
    let a = drv("/nix/store/aaaa-a.drv", &[]);
    let b = drv("/nix/store/bbbb-b.drv", &[]);
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), a, vec![]));
    s.enqueue(dag_item(rid(2), EvalId::new(2), jid(2), b, vec![]));

    let d1 = s.dispatch().unwrap();
    let _ = s.complete(d1.token, JobStatus::Success);
    let d2 = s.dispatch().unwrap();
    // After the first dispatch, the loser repo's vt is still 0 → it
    // wins the second round. So the two repos alternate.
    assert_ne!(d1.repo_id, d2.repo_id);
}

#[test]
fn dag_shared_step_with_one_failed_dep_skips_only_that_branch() {
    // X → Z and Y → Z. Z succeeds. X fails. Y must NOT be cascade-
    // skipped (Y depends on Z, not on X).
    let mut s = DagStrategy::new(None);
    let z = drv("/nix/store/zzzz-z.drv", &[]);
    let x = drv("/nix/store/xxxx-x.drv", &["/nix/store/zzzz-z.drv"]);
    let y = drv("/nix/store/yyyy-y.drv", &["/nix/store/zzzz-z.drv"]);
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), x, vec![z.clone()]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(2), y, vec![z]));

    let d_z = s.dispatch().unwrap();
    assert_eq!(d_z.drv_path, "/nix/store/zzzz-z.drv");
    let _ = s.complete(d_z.token, JobStatus::Success);

    // Now X and Y are both Ready. Pop them.
    let d1 = s.dispatch().unwrap();
    let d2 = s.dispatch().unwrap();

    // Fail one (X). The other (Y) is in flight, must not be skipped.
    let (x_d, _y_d) = if d1.head_job == Some(jid(1)) {
        (d1, d2)
    } else {
        (d2, d1)
    };
    let eff = s.complete(x_d.token, JobStatus::Failure);
    assert!(
        eff.cascaded_skips.is_empty(),
        "Y depends on Z (which succeeded), not on X — must not cascade",
    );
}

#[test]
fn dag_concurrency_cap_applies_across_steps() {
    // Cap at 1: even if multiple Steps are Ready, only one dispatches
    // at a time. This is just WfqCore's job, but we verify it still
    // works through the wider trait surface.
    let mut s = DagStrategy::new(Some(1));
    let a = drv("/nix/store/aaaa-a.drv", &[]);
    let b = drv("/nix/store/bbbb-b.drv", &[]);
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), a, vec![]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(2), b, vec![]));

    let d1 = s.dispatch().unwrap();
    assert!(s.dispatch().is_none(), "cap blocks second");
    let _ = s.complete(d1.token, JobStatus::Success);
    let d2 = s.dispatch().unwrap();
    assert_ne!(d1.drv_path, d2.drv_path);
}

#[test]
fn dag_aliased_head_drv_dispatches_once_emits_alias_completion() {
    // Two ScheduleItems with the same head_drv.drv_path — Nix flakes
    // routinely re-export the same derivation under multiple
    // attribute paths (NixOS test re-exports, `pkgs.foo` and
    // `pkgs.bar` aliasing each other, etc.). The Step is dispatched
    // exactly once; the second Job surfaces via alias_completions
    // when the primary completes, so the daemon mirrors the terminal
    // status into both DB rows + forge checks.
    let mut s = DagStrategy::new(None);
    let d = drv("/nix/store/aliased.drv", &[]);
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), d.clone(), vec![]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(2), d, vec![]));

    // Exactly one dispatch even though two Jobs were enqueued.
    let dispatched = s.dispatch().unwrap();
    assert_eq!(dispatched.drv_path, "/nix/store/aliased.drv");
    assert_eq!(dispatched.head_job, Some(jid(1)));
    assert!(s.dispatch().is_none(), "no second dispatch for the alias");

    // Complete the primary; the alias surfaces via alias_completions.
    let effects = s.complete(dispatched.token, JobStatus::Success);
    assert_eq!(effects.alias_completions.len(), 1);
    assert_eq!(effects.alias_completions[0].job_id, jid(2));
    assert_eq!(effects.alias_completions[0].eval_id, EvalId::new(1));
    assert_eq!(effects.alias_completions[0].repo_id, rid(1));
    assert!(effects.cascaded_skips.is_empty());
}

#[test]
fn dag_alias_with_three_jobs_sharing_one_drv() {
    // Three-way alias: drv → {jid1, jid2, jid3}. One dispatch, two
    // alias_completions on completion (the primary is reported via
    // Dispatched.head_job).
    let mut s = DagStrategy::new(None);
    let d = drv("/nix/store/triple.drv", &[]);
    for j in [1, 2, 3] {
        s.enqueue(dag_item(rid(1), EvalId::new(1), jid(j), d.clone(), vec![]));
    }
    let dispatched = s.dispatch().unwrap();
    assert_eq!(dispatched.head_job, Some(jid(1)));
    let effects = s.complete(dispatched.token, JobStatus::Success);
    let mut alias_ids: Vec<JobId> = effects.alias_completions.iter().map(|a| a.job_id).collect();
    alias_ids.sort_by_key(|j| j.get());
    assert_eq!(alias_ids, vec![jid(2), jid(3)]);
}

#[test]
fn dag_alias_failure_cascades_to_all_aliased_jobs() {
    // drv X aliased by jid1 + jid2. A separate top-level B depends
    // on X. When X fails, cascade_skip walks B and emits a skip; the
    // failure also flows through alias_completions for jid2 (since
    // jid1 is the primary). Net: jid1 fails (primary), jid2 fails
    // (alias), B is skipped.
    let mut s = DagStrategy::new(None);
    let x = drv("/nix/store/x.drv", &[]);
    let b = drv("/nix/store/b.drv", &["/nix/store/x.drv"]);
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(1), x.clone(), vec![]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(2), x, vec![]));
    s.enqueue(dag_item(rid(1), EvalId::new(1), jid(3), b, vec![]));

    let dispatched = s.dispatch().unwrap();
    assert_eq!(dispatched.drv_path, "/nix/store/x.drv");
    let effects = s.complete(dispatched.token, JobStatus::Failure);
    assert_eq!(effects.alias_completions.len(), 1);
    assert_eq!(effects.alias_completions[0].job_id, jid(2));
    assert_eq!(effects.cascaded_skips.len(), 1);
    assert_eq!(effects.cascaded_skips[0].job_id, jid(3));
}

// ---------------------------------------------------------------------
// cancel_eval tests
// ---------------------------------------------------------------------

#[test]
fn flat_cancel_eval_drops_pending_and_emits_skips_for_only_that_eval() {
    let mut s = FlatStrategy::new(None);
    // Eval A: jobs 1, 2, 3. Eval B: jobs 10, 11.
    for j in [1, 2, 3] {
        s.enqueue(ScheduleItem {
            repo_id: rid(1),
            eval_id: EvalId::new(7),
            job_id: jid(j),
            head_drv: drv(&format!("/nix/store/aa-{j}.drv"), &[]),
            closure: Vec::new(),
        });
    }
    for j in [10, 11] {
        s.enqueue(ScheduleItem {
            repo_id: rid(1),
            eval_id: EvalId::new(8),
            job_id: jid(j),
            head_drv: drv(&format!("/nix/store/bb-{j}.drv"), &[]),
            closure: Vec::new(),
        });
    }
    assert_eq!(s.pending_count(), 5);

    let skips = s.cancel_eval(EvalId::new(7));
    let mut got: Vec<JobId> = skips.iter().map(|s| s.job_id).collect();
    got.sort_by_key(|j| j.get());
    assert_eq!(got, vec![jid(1), jid(2), jid(3)]);
    for sk in &skips {
        assert_eq!(sk.eval_id, EvalId::new(7));
        assert_eq!(sk.repo_id, rid(1));
    }

    // Only eval B's jobs remain dispatchable.
    assert_eq!(s.pending_count(), 2);
    let mut popped: Vec<JobId> = Vec::new();
    while let Some(d) = s.dispatch() {
        popped.push(d.head_job.unwrap());
        s.complete(d.token, JobStatus::Success);
    }
    popped.sort_by_key(|j| j.get());
    assert_eq!(popped, vec![jid(10), jid(11)]);
}

#[test]
fn flat_cancel_eval_after_some_dispatched_only_drops_remaining_pending() {
    let mut s = FlatStrategy::new(Some(1));
    s.enqueue(ScheduleItem {
        repo_id: rid(1),
        eval_id: EvalId::new(7),
        job_id: jid(1),
        head_drv: drv("/nix/store/aa-1.drv", &[]),
        closure: Vec::new(),
    });
    s.enqueue(ScheduleItem {
        repo_id: rid(1),
        eval_id: EvalId::new(7),
        job_id: jid(2),
        head_drv: drv("/nix/store/aa-2.drv", &[]),
        closure: Vec::new(),
    });
    let d1 = s.dispatch().unwrap();
    assert_eq!(d1.head_job, Some(jid(1)));

    // Now cancel — job 1 is in-flight (left alone), job 2 is pending
    // (must be dropped, must surface in skips).
    let skips = s.cancel_eval(EvalId::new(7));
    assert_eq!(skips.len(), 1, "only job 2 should be in the skip list");
    assert_eq!(skips[0].job_id, jid(2));

    // Job 1 still terminates normally through complete().
    let eff = s.complete(d1.token, JobStatus::Cancelled);
    assert_eq!(eff.repo_id, Some(rid(1)));
}

#[test]
fn flat_cancel_eval_is_idempotent() {
    let mut s = FlatStrategy::new(None);
    s.enqueue(ScheduleItem {
        repo_id: rid(1),
        eval_id: EvalId::new(7),
        job_id: jid(1),
        head_drv: drv("/nix/store/x.drv", &[]),
        closure: Vec::new(),
    });
    let first = s.cancel_eval(EvalId::new(7));
    assert_eq!(first.len(), 1);
    let second = s.cancel_eval(EvalId::new(7));
    assert!(second.is_empty(), "second call has nothing left to skip");
}

#[test]
fn dag_cancel_eval_skips_head_and_cascades_through_rdeps() {
    // Eval 7: A → B → C (chain of 3 head Jobs). Cancel — all 3 must
    // surface as skips, none must dispatch.
    let mut s = DagStrategy::new(None);
    let a = drv("/nix/store/aaaa.drv", &[]);
    let b = drv("/nix/store/bbbb.drv", &["/nix/store/aaaa.drv"]);
    let c = drv("/nix/store/cccc.drv", &["/nix/store/bbbb.drv"]);
    s.enqueue(dag_item(rid(1), EvalId::new(7), jid(1), a.clone(), vec![]));
    s.enqueue(dag_item(
        rid(1),
        EvalId::new(7),
        jid(2),
        b.clone(),
        vec![a.clone()],
    ));
    s.enqueue(dag_item(rid(1), EvalId::new(7), jid(3), c, vec![a, b]));

    let skips = s.cancel_eval(EvalId::new(7));
    let mut got: Vec<JobId> = skips.iter().map(|s| s.job_id).collect();
    got.sort_by_key(|j| j.get());
    assert_eq!(got, vec![jid(1), jid(2), jid(3)]);

    // No dispatches happen — A was the only ready Step and it's now
    // Cancelled. The strategy is fully drained.
    assert!(s.dispatch().is_none());
}

#[test]
fn dag_cancel_eval_keeps_internal_step_for_other_live_eval() {
    // Eval 7: head X depends on internal Z.
    // Eval 8: head Y depends on internal Z.
    // Z is shared — only one Step.
    // Cancel eval 7 → X is skipped; Z stays Pending/Ready (still wanted
    // by Y); Y eventually dispatches once Z succeeds.
    let mut s = DagStrategy::new(None);
    let z = drv("/nix/store/zzzz.drv", &[]);
    let x = drv("/nix/store/xxxx.drv", &["/nix/store/zzzz.drv"]);
    let y = drv("/nix/store/yyyy.drv", &["/nix/store/zzzz.drv"]);
    s.enqueue(dag_item(rid(1), EvalId::new(7), jid(1), x, vec![z.clone()]));
    s.enqueue(dag_item(rid(1), EvalId::new(8), jid(2), y, vec![z]));

    // Cancel eval 7: only X surfaces as a skip.
    let skips = s.cancel_eval(EvalId::new(7));
    assert_eq!(skips.len(), 1);
    assert_eq!(skips[0].job_id, jid(1));

    // Z is the only ready Step; dispatch it.
    let d_z = s.dispatch().unwrap();
    assert_eq!(d_z.drv_path, "/nix/store/zzzz.drv");
    assert_eq!(d_z.head_job, None, "Z is internal");
    let _ = s.complete(d_z.token, JobStatus::Success);

    // Y becomes ready (eval 8's head); X stays cancelled.
    let next = s.dispatch().unwrap();
    assert_eq!(next.head_job, Some(jid(2)));
    assert!(
        s.dispatch().is_none(),
        "X was cancelled — no dispatch for it"
    );
}

#[test]
fn dag_cancel_eval_leaves_running_head_to_route_through_complete() {
    // Eval 7: a single top-level Job. Dispatch it, then cancel. The
    // running Step is *not* re-emitted as a skip (the daemon's
    // CancelToken handles it, and complete() will eventually be
    // called with Cancelled status). cancel_eval must report no skip.
    let mut s = DagStrategy::new(None);
    let a = drv("/nix/store/aaaa.drv", &[]);
    s.enqueue(dag_item(rid(1), EvalId::new(7), jid(1), a, vec![]));
    let d = s.dispatch().unwrap();
    assert_eq!(d.head_job, Some(jid(1)));

    let skips = s.cancel_eval(EvalId::new(7));
    assert!(
        skips.is_empty(),
        "running head Step is not a skip target; complete() handles it",
    );

    // Simulate the daemon's CancelToken signalling the build to abort.
    let eff = s.complete(d.token, JobStatus::Cancelled);
    assert_eq!(eff.repo_id, Some(rid(1)));
}
