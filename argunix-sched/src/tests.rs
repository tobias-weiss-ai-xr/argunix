use super::*;
use argunix_domain::{EvalId, JobStatus};
use std::time::Duration;

fn jid(n: i64) -> JobId {
    JobId::new(n)
}

fn rid(n: i64) -> RepoId {
    RepoId::new(n)
}

fn item(repo: RepoId, job: JobId) -> ScheduleItem {
    ScheduleItem {
        repo_id: repo,
        eval_id: EvalId::new(0),
        job_id: job,
        drv_path: None,
        system: None,
        required_features: Vec::new(),
        input_drvs: None,
    }
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

    assert_eq!(s.dispatch().unwrap().job_id, jid(10));
    assert_eq!(s.dispatch().unwrap().job_id, jid(11));
    assert_eq!(s.dispatch().unwrap().job_id, jid(12));
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
    let d1 = s.dispatch().unwrap();
    assert_eq!(d1.repo_id, rid(1));
    s.complete(d1.job_id, JobStatus::Success);

    // Repo 1's vt is now 1.0; repo 2 still at 0 → repo 2 wins.
    let d2 = s.dispatch().unwrap();
    assert_eq!(d2.repo_id, rid(2));
    s.complete(d2.job_id, JobStatus::Success);

    // Both back at vt=1.0; tie broken by remaining earliest seq → repo 1.
    let d3 = s.dispatch().unwrap();
    assert_eq!(d3.repo_id, rid(1));
    s.complete(d3.job_id, JobStatus::Success);

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
            s.complete(d.job_id, JobStatus::Success);
        }
    }

    let ratio = a as f64 / b as f64;
    // Expected exactly 2.0 in the limit; allow ±5% slop for the tail.
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
            s.complete(d.job_id, JobStatus::Success);
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

    s.complete(d1.job_id, JobStatus::Success);
    let d3 = s.dispatch().unwrap();
    assert_eq!(d3.job_id, jid(3));
    s.complete(d2.job_id, JobStatus::Success);
    s.complete(d3.job_id, JobStatus::Success);
    assert!(s.dispatch().is_none());
    assert_eq!(s.in_flight_count(), 0);
}

#[test]
fn complete_returns_repo_for_known_jobs_and_none_otherwise() {
    let mut s = FlatStrategy::new(None);
    s.enqueue(item(rid(7), jid(100)));
    let d = s.dispatch().unwrap();
    assert_eq!(s.complete(d.job_id, JobStatus::Success), Some(rid(7)));
    assert_eq!(s.complete(d.job_id, JobStatus::Success), None);
    assert_eq!(s.complete(jid(999_999), JobStatus::Success), None);
}

#[test]
fn idle_repo_does_not_advance_in_virtual_time() {
    // Repo 1 has no pending; its virtual_time stays at 0.
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
    // Repo 1 dispatches many times alone, advancing its vt. Then repo 2
    // arrives. Without the system-vt snap, repo 2 would dispatch all of
    // its work before repo 1 again — which would starve repo 1. With the
    // snap, repo 2 enters at repo 1's current vt and they alternate.
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 1);
    s.set_weight(rid(2), 1);
    for n in 0..50 {
        s.enqueue(item(rid(1), jid(n)));
    }
    for _ in 0..30 {
        let d = s.dispatch().unwrap();
        s.complete(d.job_id, JobStatus::Success);
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
    // Stress test: 5 repos with mixed weights, deep queues, drive the
    // scheduler for many rounds and verify every repo's queue eventually
    // empties.
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
            s.complete(d.job_id, JobStatus::Success);
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
    // Both repos same weight, both at vt=0. Repo 2 enqueues first → wins
    // the tie even though IDs would suggest repo 1.
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
    // Repo 1: weight 10, deep queue, dispatching for a while.
    // Repo 2: weight 1, arrives later.
    // Verify repo 2 still gets *some* dispatches in a reasonable horizon.
    let mut s = FlatStrategy::new(None);
    s.set_weight(rid(1), 10);
    s.set_weight(rid(2), 1);
    for n in 0..200 {
        s.enqueue(item(rid(1), jid(n)));
    }
    for _ in 0..50 {
        let d = s.dispatch().unwrap();
        s.complete(d.job_id, JobStatus::Success);
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
            s.complete(d.job_id, JobStatus::Success);
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
    assert_eq!(d.job_id, jid(1));
    assert!(s.dispatch().is_none(), "cap should block second dispatch");
    s.complete(d.job_id, JobStatus::Success);
    let d2 = s.dispatch().unwrap();
    assert_eq!(d2.job_id, jid(2));
}
