//! The heavy-job gate: one import, export or Stats page per user at a
//! time, a few across the process, released on drop; and the import
//! row cap the permit-holding preview enforces.

use std::sync::Arc;

use flash_server::flows::import_flow;
use flash_server::state::{HeavyBusy, HeavyJobs};
use flash_server::testing::{data_dir, member, AppBuilder};

#[test]
fn one_job_per_user_and_a_few_per_process() {
    let t = AppBuilder::new("heavy-gate").build();
    let users: Vec<_> = (0..5)
        .map(|i| member(&t.store, &format!("U{i}"), &format!("u{i}@example.com")))
        .collect();
    let jobs = Arc::new(HeavyJobs::new(4));

    let first = jobs.acquire(users[0]).unwrap();
    assert_eq!(jobs.acquire(users[0]).unwrap_err(), HeavyBusy::Yours);
    let others: Vec<_> = users[1..4]
        .iter()
        .map(|u| jobs.acquire(*u).unwrap())
        .collect();
    assert_eq!(jobs.inflight(), 4);
    assert_eq!(jobs.acquire(users[4]).unwrap_err(), HeavyBusy::Everyone);
    // A refused user is not left marked as running.
    drop(first);
    assert_eq!(jobs.inflight(), 3);
    let again = jobs.acquire(users[4]).unwrap();
    assert_eq!(jobs.inflight(), 4);
    drop(again);
    drop(others);
    assert_eq!(jobs.inflight(), 0);
    assert!(jobs.acquire(users[0]).is_ok());
}

#[test]
fn an_import_over_the_row_cap_is_refused_before_it_is_parked() {
    let t = AppBuilder::new("heavy-rows").build();
    let user = member(&t.store, "FlashTester", "flashtester@example.com");
    let jobs = Arc::new(HeavyJobs::new(1));
    let permit = jobs.acquire(user).unwrap();
    let dir = data_dir("heavy-rows-import");
    std::fs::create_dir_all(&dir).unwrap();

    let mut csv = String::from("front,back\n");
    for i in 0..=import_flow::MAX_ROWS {
        csv.push_str(&format!("q{i},a{i}\n"));
    }
    let err = import_flow::preview(&permit, &dir, "big.csv", csv.as_bytes(), "").unwrap_err();
    assert!(err.to_string().contains("capped"), "{err}");
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "nothing parked for a refused file"
    );

    let small = "front,back\nq,a\n";
    assert!(import_flow::preview(&permit, &dir, "small.csv", small.as_bytes(), "").is_ok());
}
