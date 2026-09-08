//! A panic while the connection is locked costs that caller and nothing
//! after it. The store's mutex has no poisoned state, and a transaction
//! open at the moment of the panic rolls back as its guard unwinds, so
//! the next caller finds a working, clean connection. Without this, one
//! reachable panic under the lock would turn every later request into a
//! panic for as long as the process lived.

use std::panic::{catch_unwind, AssertUnwindSafe};

use flash_store::{Store, StoreError};

#[test]
fn a_panic_under_the_lock_does_not_poison_it() {
    let store = Store::open_in_memory().unwrap();

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        store.with_conn(|_| -> Result<(), StoreError> { panic!("boom under the lock") })
    }));
    assert!(outcome.is_err(), "the panic propagates to the caller");

    // The next caller is not the one who panicked; the lock serves them.
    store.health_check().expect("the connection still answers");
    let admin = store
        .create_user("Ada", Some("ada@example.com"), "admin", 1)
        .expect("writes still work");
    assert!(admin.raw() > 0);
}

#[test]
fn a_panic_inside_a_transaction_rolls_it_back() {
    let store = Store::open_in_memory().unwrap();

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        store.with_tx(|tx| -> Result<(), StoreError> {
            tx.execute(
                "INSERT INTO users (display_name, email, role, created_at) VALUES ('Ghost', 'ghost@example.com', 'member', 1)",
                [],
            )?;
            panic!("boom mid-transaction")
        })
    }));
    assert!(outcome.is_err());

    let ghosts: i64 = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM users WHERE email = 'ghost@example.com'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(ghosts, 0, "the half-done write was rolled back");
}
