//! A1–A9: the channel contract.
//!
//! Every test names the witness a *wrong* implementation fails. Three rules
//! carried from M5.1, each of which cost a real defect there:
//!
//! - a witness on a fixture that cannot move is not a witness;
//! - a refusal must be paired with the same operation succeeding, or it passes
//!   by refusing everyone;
//! - assert the value, not the absence of a value you guessed at.

use std::sync::Arc;
use townhall_channel::{
    ChannelAddress, ChannelConfig, ChannelError, ChannelKind, Command, ControlCommand,
    HumanChannel, InboundBody, InboundIdentity, MessageReceipt, OutboundMessage, RawInbound,
    Region, ReplayWindow, ResourceCommand, Seen, SmsSimulator, SuppressionStore as _,
    TransportEvidence, body, classify, simulator::InMemorySuppression,
};

// ------------------------------------------------------------------ A1: addresses

/// Three formats reach one address; a different region reaches a different one;
/// and the rejections are refusals, not lenient successes.
#[test]
fn a1_addresses_normalize_against_a_configured_region() {
    let canonical = "+447700900123";
    for raw in [
        "07700900123",
        "+44 7700 900123",
        "+447700900123",
        "(07700) 900-123",
    ] {
        let parsed = ChannelAddress::parse(raw, Region::Gb).expect(raw);
        assert_eq!(parsed.revealed(), canonical, "{raw} normalized wrongly");
    }

    // The same national digits under a different context are a different
    // subscriber. Without this, "region-aware" is a word rather than a
    // behaviour — an implementation hardcoding +44 passes every row above.
    let us = ChannelAddress::parse("17005550123", Region::Us).expect("us number");
    assert_eq!(us.revealed(), "+17005550123");
    assert_ne!(
        us.revealed(),
        ChannelAddress::parse("07005550123", Region::Gb)
            .expect("gb number")
            .revealed()
    );

    for bad in [
        "12345",            // too short to be a subscriber
        "+44 77",           // ditto, international
        "+44abc",           // not digits
        "",                 // nothing at all
        "+44 07700 900123", // a `+` number that kept its trunk zero
    ] {
        assert!(
            matches!(
                ChannelAddress::parse(bad, Region::Gb),
                Err(ChannelError::UnroutableAddress(_))
            ),
            "{bad:?} was accepted; a half-parsed number fails equality later, silently"
        );
    }
}

// ------------------------------------------------------------------ A2: bounds

/// The body rejects past its limit, never truncates, and preserves what it took.
#[test]
fn a2_inbound_body_rejects_and_preserves() {
    let at_limit: String = "a".repeat(1600);
    assert!(InboundBody::parse(&at_limit).is_ok());

    let over: String = "a".repeat(1601);
    assert!(
        matches!(
            InboundBody::parse(&over),
            Err(ChannelError::TooLong {
                scalars: 1601,
                limit: 1600
            })
        ),
        "asserting the ERROR: an implementation that truncated and returned Ok \
         satisfies any length check on the result"
    );

    // The discriminating half. 600 emoji is 2400 bytes — past
    // BoundedString's 512-byte cap, which truncates and returns Ok. "It
    // returned Ok" would pass for that implementation; byte-for-byte equality
    // will not.
    let emoji: String = "😀".repeat(600);
    let parsed = InboundBody::parse(&emoji).expect("600 scalars is within 1600");
    assert_eq!(parsed.revealed(), emoji, "the body was silently shortened");
    assert_eq!(parsed.len_scalars(), 600);
}

// ------------------------------------------------------------------ A3: segments

/// Segment counting, across the real alphabet and at every boundary.
#[test]
fn a3_segment_counting_is_exact() {
    let gsm = |n: usize| "a".repeat(n);
    for (chars, expected) in [(160, 1), (161, 2), (306, 2), (307, 3)] {
        assert_eq!(
            body::segment(&gsm(chars)).segments,
            expected,
            "GSM-7 {chars} characters"
        );
    }

    // The whole basic table, iterated — not a sample. An "including…" list
    // cannot catch an implementation that omits precisely the characters nobody
    // thought to name.
    assert_eq!(
        body::GSM_BASIC.len(),
        128,
        "the basic table must be complete, or iterating it proves nothing"
    );
    for character in body::GSM_BASIC {
        let single = character.to_string();
        let counted = body::segment(&single);
        assert_eq!(
            counted.alphabet,
            body::Alphabet::Gsm7,
            "{character:?} (U+{:04X}) is basic GSM and must not force UCS-2",
            character as u32
        );
        // 160 of a one-septet character is exactly one segment; 161 is two.
        assert_eq!(
            body::segment(&single.repeat(160)).segments,
            1,
            "{character:?} should cost one septet"
        );
        assert_eq!(body::segment(&single.repeat(161)).segments, 2);
    }

    // The extension table costs two septets each, form feed included.
    for character in body::GSM_EXTENSION {
        let single = character.to_string();
        assert_eq!(
            body::segment(&single.repeat(80)).segments,
            1,
            "{character:?} × 80 is exactly 160 septets"
        );
        assert_eq!(
            body::segment(&single.repeat(81)).segments,
            2,
            "{character:?} × 81 crosses 160 — a character-counting implementation \
             passes every earlier row and fails here"
        );
    }

    // `£` is BASIC, not extension: one septet. Lumping it with `€` is the
    // natural mistake and would make 160 of them two segments.
    assert_eq!(body::segment(&"£".repeat(160)).segments, 1);

    // Anything outside both tables drags the WHOLE message to UCS-2.
    let cyrillic = body::segment("Ж");
    assert_eq!(cyrillic.alphabet, body::Alphabet::Ucs2);
    for (units, expected) in [(70, 1), (71, 2), (134, 2), (135, 3)] {
        assert_eq!(
            body::segment(&"Ж".repeat(units)).segments,
            expected,
            "UCS-2 {units} code units"
        );
    }

    // A supplementary-plane character is TWO UTF-16 code units. Counting
    // scalars here would make 35 emoji look like half a segment.
    assert_eq!(
        body::segment(&"😀".repeat(35)).segments,
        1,
        "35 emoji = 70 code units = exactly one segment"
    );
    assert_eq!(
        body::segment(&"😀".repeat(36)).segments,
        2,
        "36 emoji = 72 code units — a scalar counter says 36 and stays at one"
    );
}

// ------------------------------------------------------------------ A4: truncation

/// Truncation fits the ceiling with the marker inside it, on a char boundary.
#[test]
fn a4_outbound_truncation_reserves_room_for_its_marker() {
    let long = "a".repeat(1000);
    let (fitted, truncated) = body::fit(&long, 2);
    assert!(truncated);
    assert!(fitted.ends_with('…'), "the marker must survive the cut");

    // Recounted by the test, independently of the production counter — a wrong
    // counter agrees with itself, so asking it twice proves nothing.
    let septets = fitted
        .chars()
        .map(|c| {
            if body::GSM_EXTENSION.contains(&c) {
                2
            } else {
                1
            }
        })
        .sum::<usize>();
    let independent = if septets <= 160 {
        1
    } else {
        septets.div_ceil(153)
    };
    assert!(
        independent <= 2,
        "the delivered text is {septets} septets, which is more than 2 segments"
    );

    // A multi-byte character at the cut point must not be split.
    let emoji = "😀".repeat(200);
    let (cut, was_truncated) = body::fit(&emoji, 1);
    assert!(was_truncated);
    assert!(
        cut.chars().all(|c| c == '😀' || c == '…'),
        "a scalar was split"
    );
    assert_eq!(body::segment(&cut).segments, 1);

    // Text already inside the ceiling is returned untouched.
    let (short, untouched) = body::fit("hello", 3);
    assert_eq!(short, "hello");
    assert!(!untouched);
}

// ------------------------------------------------------------------ A5: dedupe

/// Dedupe keys on the identity, and the check-and-write is one operation.
#[test]
fn a5_dedupe_keys_on_identity_atomically() {
    let window = ReplayWindow::new(1_000);
    let first = InboundIdentity::new("sim", "acct", "msg-1");

    assert_eq!(window.insert_if_absent(&first, 0), Seen::Accepted);
    assert_eq!(
        window.insert_if_absent(&first, 10),
        Seen::Duplicate,
        "the same provider message id must not be handled twice"
    );

    // Same body, different id: a genuinely new message. An implementation
    // keying on content fails here — and content-keying is the tempting
    // shortcut, because it also passes the row above.
    let second = InboundIdentity::new("sim", "acct", "msg-2");
    assert_eq!(window.insert_if_absent(&second, 10), Seen::Accepted);

    // Past the window, a redelivery is somebody texting again.
    assert_eq!(window.insert_if_absent(&first, 2_000), Seen::Accepted);
}

/// The CAS is what makes concurrent redelivery safe.
///
/// Two carrier retries arriving together fit inside the gap between a `contains`
/// and an `insert`: both look unseen, both proceed, the booking happens twice.
/// Driving many concurrent callers at one identity asserts the invariant that
/// gap would break — exactly one `Accepted`, whatever the interleaving.
#[test]
fn a5_concurrent_redelivery_admits_exactly_one() {
    let window = Arc::new(ReplayWindow::new(60_000));
    let identity = InboundIdentity::new("sim", "acct", "msg-race");

    let accepted = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let window = Arc::clone(&window);
                let identity = identity.clone();
                scope.spawn(move || window.insert_if_absent(&identity, 0))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("no panic"))
            .filter(|seen| *seen == Seen::Accepted)
            .count()
    });

    assert_eq!(
        accepted, 1,
        "exactly one caller may win; a check-then-insert lets several through"
    );
}

// ------------------------------------------------------------------ A6/A7: grammar

/// Classification, not interpretation — and strictly.
#[test]
fn a6_classification_is_strict_and_shallow() {
    for (text, expected) in [
        ("HELP", ControlCommand::Help),
        ("help", ControlCommand::Help),
        ("  Stop  ", ControlCommand::Stop),
        ("BALANCE", ControlCommand::Balance),
        ("START", ControlCommand::Start),
        ("REVOKE", ControlCommand::Revoke),
    ] {
        assert_eq!(
            classify(text),
            Command::Control(expected),
            "{text:?} should be a channel control"
        );
    }

    // The discriminating rows: a `contains()` implementation reads the first
    // two as commands and silently swallows a business request.
    for text in ["STOP the booking", "please help", "I need to cancel"] {
        assert_eq!(
            classify(text),
            Command::Freeform,
            "{text:?} is a sentence, not a command"
        );
    }

    // BOOK is Freeform. The channel must not decide it is a booking request —
    // §14 forbids this crate owning booking vocabulary.
    assert_eq!(
        classify("BOOK date=2026-09-10 people=20"),
        Command::Freeform,
        "the channel parsed booking fields it has no business knowing"
    );
}

/// Resource commands carry their argument as opaque text, unvalidated and
/// un-looked-up — this crate cannot reach anything that could look one up.
#[test]
fn a7_resource_arguments_are_carried_not_interpreted() {
    assert_eq!(
        classify("CANCEL TH-92718"),
        Command::Resource(ResourceCommand::Cancel {
            reference: "TH-92718".to_owned()
        })
    );
    // Case is preserved: a council reference is someone else's namespace.
    assert_eq!(
        classify("cancel th-92718"),
        Command::Resource(ResourceCommand::Cancel {
            reference: "th-92718".to_owned()
        })
    );
    assert_eq!(
        classify("STATUS"),
        Command::Resource(ResourceCommand::Status { reference: None })
    );
    assert_eq!(
        classify("STATUS TH-1"),
        Command::Resource(ResourceCommand::Status {
            reference: Some("TH-1".to_owned())
        })
    );

    // Malformed commands are not commands — and a WORD is not a reference.
    // "CANCEL it" must reach the path that can ask which booking (§14.1), not a
    // lookup for a booking named "it".
    for text in [
        "CANCEL",
        "STATUS a b",
        "CANCEL a b c",
        "CANCEL it",
        "STATUS it",
    ] {
        assert_eq!(
            classify(text),
            Command::Freeform,
            "{text:?} is malformed and must fall through to something that can ask"
        );
    }
}

// ------------------------------------------------------------------ A8: delivery

fn simulator() -> (SmsSimulator, Arc<InMemorySuppression>, ChannelAddress) {
    let suppression = Arc::new(InMemorySuppression::default());
    let channel = SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression) as Arc<dyn townhall_channel::SuppressionStore>,
    );
    let lucy = ChannelAddress::parse("+447700900123", Region::Gb).expect("address");
    (channel, suppression, lucy)
}

/// The three delivery outcomes, each driven rather than merely constructed.
#[tokio::test]
async fn a8_delivery_outcomes_are_driven_not_just_typed() {
    let (channel, suppression, lucy) = simulator();

    let delivered = channel
        .send(&lucy, OutboundMessage::reply("hello"))
        .await
        .expect("send");
    assert_eq!(
        delivered,
        MessageReceipt::Delivered {
            segments: 1,
            truncated: false
        }
    );

    // Suppressed: automated only, and only while suppressed.
    suppression.suppress(&lucy);
    assert_eq!(
        channel
            .send(&lucy, OutboundMessage::automated("progress"))
            .await
            .expect("send"),
        MessageReceipt::Suppressed
    );
    // A reply still gets through — if STOP silenced replies, someone who texted
    // STOP could never discover START.
    assert!(matches!(
        channel
            .send(&lucy, OutboundMessage::reply("answering you"))
            .await
            .expect("send"),
        MessageReceipt::Delivered { .. }
    ));
    suppression.allow(&lucy);
    assert!(matches!(
        channel
            .send(&lucy, OutboundMessage::automated("progress"))
            .await
            .expect("send"),
        MessageReceipt::Delivered { .. }
    ));

    // Failed: armed, so the outcome is reachable at all.
    channel.fail_next_sends(&lucy, 1);
    assert!(matches!(
        channel
            .send(&lucy, OutboundMessage::reply("this one breaks"))
            .await
            .expect("send"),
        MessageReceipt::Failed { .. }
    ));

    // Every outcome is in the record, including the ones that sent nothing: a
    // withheld message is a fact about the conversation.
    let outbox = channel.outbox();
    assert_eq!(outbox.len(), 5);
    assert_eq!(outbox[1].receipt, MessageReceipt::Suppressed);
    assert!(matches!(outbox[4].receipt, MessageReceipt::Failed { .. }));
}

// ------------------------------------------------------------------ A9: redaction

/// Redaction asserted as an exact rendering, not as the absence of a guess.
///
/// "No hash of the body appears" is not a test anyone can write — you cannot
/// enumerate the algorithms. Equality against the complete expected string is,
/// and it fails on any addition.
#[test]
fn a9_debug_renderings_are_exactly_these() {
    let address = ChannelAddress::parse("+447700900123", Region::Gb).expect("address");
    assert_eq!(format!("{address:?}"), r#"ChannelAddress("+4477…0123")"#);
    // Display must agree: a type with a safe Debug and a leaky Display is worse
    // than one with neither, because it looks handled.
    assert_eq!(format!("{address}"), r#"ChannelAddress("+4477…0123")"#);

    // A four-digit approval code has ten thousand possible bodies, so any
    // unkeyed digest of it is an encoding, not a concealment.
    let body = InboundBody::parse("YES 7312").expect("body");
    assert_eq!(format!("{body:?}"), "InboundBody(len=8 scalars)");

    let evidence =
        TransportEvidence::new("sim", "+447700900123", true).with_signature("deadbeefcafe");
    assert_eq!(
        format!("{evidence:?}"),
        r#"TransportEvidence { provider: "sim", verified: true }"#
    );
}

// ------------------------------------------------------------------ receive()

/// The whole inbound path, in the order that matters.
#[tokio::test]
async fn receive_normalizes_bounds_then_dedupes() {
    let (channel, _suppression, _lucy) = simulator();
    let raw = |id: &str, from: &str, text: &str| RawInbound {
        identity: InboundIdentity::new("sim", "acct", id),
        channel: ChannelKind::SmsSimulator,
        from: from.to_owned(),
        body: text.to_owned(),
        received_at_ms: 0,
        evidence: TransportEvidence::new("sim", from, true),
    };

    let message = channel
        .receive(raw("m1", "07700 900123", "HELP"))
        .await
        .expect("accepted");
    assert_eq!(message.address.revealed(), "+447700900123");
    assert_eq!(message.body.revealed(), "HELP");

    assert!(
        matches!(
            channel.receive(raw("m1", "07700900123", "HELP")).await,
            Err(ChannelError::Duplicate)
        ),
        "a carrier retry must not be handled twice"
    );

    // An unroutable address is refused BEFORE the window is spent on it —
    // otherwise a malformed flood evicts real entries.
    assert!(matches!(
        channel.receive(raw("m2", "nonsense", "HELP")).await,
        Err(ChannelError::UnroutableAddress(_))
    ));
    // Proof it was not consumed: the same id is still usable.
    assert!(
        channel
            .receive(raw("m2", "07700900123", "HELP"))
            .await
            .is_ok()
    );
}

// ------------------------------------------------------------------ M6A gate (b)

/// **M6A's gate, clause (b).** The complete channel contract, end to end.
///
/// Clause (a) — the gateway journey — never touches this crate, so on its own it
/// would let a missing or incompatible channel half pass. This is the other
/// half: one continuous run through everything the channel owes, in-process.
#[tokio::test]
async fn m6a_gate_b_the_complete_channel_contract() {
    let (channel, suppression, lucy) = simulator();
    let raw = |id: &str, from: &str, text: &str| RawInbound {
        identity: InboundIdentity::new("sim", "acct", id),
        channel: ChannelKind::SmsSimulator,
        from: from.to_owned(),
        body: text.to_owned(),
        received_at_ms: 0,
        evidence: TransportEvidence::new("sim", from, true),
    };

    // Normalized: three spellings of Lucy's number are one address.
    let first = channel
        .receive(raw("g1", "07700 900123", "STATUS"))
        .await
        .expect("accepted");
    let second = channel
        .receive(raw("g2", "+44 7700 900123", "HELP"))
        .await
        .expect("accepted");
    assert_eq!(first.address, second.address);

    // Bounded: over the cap is refused, not shortened.
    assert!(matches!(
        channel
            .receive(raw("g3", "07700900123", &"x".repeat(1601)))
            .await,
        Err(ChannelError::TooLong { .. })
    ));

    // Deduped: a carrier retry of g1 is refused; a NEW message with the same
    // body is not — the identity is the message, not the words.
    assert!(matches!(
        channel.receive(raw("g1", "07700900123", "STATUS")).await,
        Err(ChannelError::Duplicate)
    ));
    assert!(
        channel
            .receive(raw("g4", "07700900123", "STATUS"))
            .await
            .is_ok()
    );

    // Classified, one of each arm — the dispatcher's whole input vocabulary.
    assert!(matches!(
        classify(first.body.revealed()),
        Command::Resource(ResourceCommand::Status { reference: None })
    ));
    assert!(matches!(
        classify(second.body.revealed()),
        Command::Control(ControlCommand::Help)
    ));
    let book = channel
        .receive(raw("g5", "07700900123", "BOOK date=2026-09-10 people=20"))
        .await
        .expect("accepted");
    assert_eq!(classify(book.body.revealed()), Command::Freeform);

    // Outbound: segmented, truncated where necessary, receipt honest.
    let long_reply = OutboundMessage::reply("a".repeat(1000));
    let receipt = channel.send(&lucy, long_reply).await.expect("send");
    let MessageReceipt::Delivered {
        segments,
        truncated,
    } = receipt
    else {
        panic!("expected delivery: {receipt:?}");
    };
    assert!(truncated, "1000 chars exceeds a 3-segment ceiling");
    assert!(segments <= 3);
    let sent = channel.outbox().pop().expect("recorded");
    assert!(sent.text.ends_with('…'));

    // Suppressed where suppressed — and only the automated class.
    suppression.suppress(&lucy);
    assert_eq!(
        channel
            .send(&lucy, OutboundMessage::automated("progress"))
            .await
            .expect("send"),
        MessageReceipt::Suppressed
    );
    assert!(matches!(
        channel
            .send(&lucy, OutboundMessage::reply("your answer"))
            .await
            .expect("send"),
        MessageReceipt::Delivered { .. }
    ));
}

// ------------------------------------------------------------------ the seam

/// The exact values M6B's dispatcher will match on, pinned structurally.
///
/// M6A and M6B are separate slices, so the seam between them is where drift
/// would land: the channel starts producing a shape the dispatcher no longer
/// expects, and both suites stay green because neither looks across. This test
/// IS the look across — every arm, every payload, as data.
#[test]
fn the_seam_m6b_consumes_is_exactly_this() {
    let cases: Vec<(&str, Command)> = vec![
        ("HELP", Command::Control(ControlCommand::Help)),
        ("BALANCE", Command::Control(ControlCommand::Balance)),
        ("STOP", Command::Control(ControlCommand::Stop)),
        ("START", Command::Control(ControlCommand::Start)),
        ("REVOKE", Command::Control(ControlCommand::Revoke)),
        (
            "CANCEL TH-92718",
            Command::Resource(ResourceCommand::Cancel {
                reference: "TH-92718".to_owned(),
            }),
        ),
        (
            "STATUS",
            Command::Resource(ResourceCommand::Status { reference: None }),
        ),
        (
            "STATUS TH-92718",
            Command::Resource(ResourceCommand::Status {
                reference: Some("TH-92718".to_owned()),
            }),
        ),
        (
            "BOOK date=2026-09-10 from=14:00 to=17:00 people=20",
            Command::Freeform,
        ),
        ("Cancel it", Command::Freeform),
    ];
    for (text, expected) in cases {
        assert_eq!(classify(text), expected, "the seam moved under {text:?}");
    }
}
