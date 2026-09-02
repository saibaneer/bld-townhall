//! The deterministic concurrency witness — every interleaving, model-checked.
//!
//! Runs ONLY under `--features loom`, which CI does as its own step.
//! The review that demanded this was right on two counts a lighter defence
//! missed: a probabilistic race passes a broken check-then-insert by scheduler
//! mood, and a public-API scan stays green if `insert_if_absent` is rewritten
//! with two lock acquisitions INSIDE — loom regresses both, because it explores
//! every schedule the modelled lock permits and fails on the one where two
//! callers are both told `Accepted`.
#![cfg(feature = "loom")]

use townhall_channel::{InboundIdentity, ReplayWindow, Seen};

#[test]
fn every_interleaving_admits_exactly_one() {
    loom::model(|| {
        let window = loom::sync::Arc::new(ReplayWindow::new(60_000));
        let identity = InboundIdentity::new("sim", "acct", "msg-race");

        let first = {
            let window = loom::sync::Arc::clone(&window);
            let identity = identity.clone();
            loom::thread::spawn(move || window.insert_if_absent(&identity, 0))
        };
        let second = window.insert_if_absent(&identity, 0);
        let first = first.join().expect("no panic");

        let accepted = [first, second]
            .iter()
            .filter(|seen| **seen == Seen::Accepted)
            .count();
        assert_eq!(
            accepted, 1,
            "an interleaving admitted {accepted} callers for one identity"
        );
    });
}
