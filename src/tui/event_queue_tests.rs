use super::*;
use futures_util::FutureExt;
use std::time::Duration;

fn drain(rx: &mut EventReceiver) -> Vec<UiEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn ordinary_unicode_transcript_keeps_every_delta_in_order() {
    let (tx, mut rx) = channel();
    for i in 0..320 {
        tx.send(UiEvent::Text(format!("你好{i}🦀"))).unwrap();
    }
    for i in 0..320 {
        assert!(matches!(rx.try_recv(), Ok(UiEvent::Text(text)) if text == format!("你好{i}🦀")));
    }
    assert_eq!(rx.try_recv().err(), Some(TryRecvError::Empty));
}

#[test]
fn count_and_allocated_byte_limits_pause_instead_of_splicing_lost_text() {
    let (tx, mut rx) = channel();
    for _ in 0..MAX_EVENTS + 100 {
        tx.send(UiEvent::Text("你好🦀".into())).unwrap();
    }
    {
        let state = tx.shared.state.lock().unwrap();
        assert_eq!(state.queue.len(), MAX_EVENTS);
        assert!(state.bytes <= MAX_BYTES);
        assert!(state.overloaded);
    }
    let events = drain(&mut rx);
    assert_eq!(events.len(), MAX_EVENTS + 1);
    assert!(matches!(events.last(), Some(UiEvent::BacklogOverflow)));
    tx.send(UiEvent::Text("do not append across a lost span".into()))
        .unwrap();
    assert!(drain(&mut rx).is_empty());
    tx.send(UiEvent::TaskStarted).unwrap();
    let mut oversize = String::with_capacity(MAX_BYTES + 1);
    oversize.push('小');
    tx.send(UiEvent::Text(oversize)).unwrap();
    let state = tx.shared.state.lock().unwrap();
    assert!(
        state.queue.is_empty(),
        "allocated capacity, not just text length, is bounded"
    );
    assert_eq!(state.bytes, 0);
    assert!(state.overloaded);
}

#[test]
fn overload_keeps_exact_accounting_latest_context_and_final_idle() {
    let (tx, mut rx) = channel();
    let mut expected = UsageMeter::default();
    let mut expected_apec = UsageMeter::default();
    tx.send(UiEvent::TaskStarted).unwrap();
    tx.send(UiEvent::Text("x".repeat(MAX_BYTES))).unwrap();
    for i in 0..20_000 {
        let usage = Usage {
            input: Some(i),
            output: (i % 5 != 0).then_some(i + 1),
            cached: (i % 2 == 0).then_some(i / 2),
            reasoning: (i % 3 == 0).then_some(4),
            total: None,
        };
        expected.record(Some(&usage));
        tx.send(UiEvent::Usage {
            input: usage.input,
            output: usage.output,
            cached: usage.cached,
            reasoning: usage.reasoning,
        })
        .unwrap();
        tx.send(UiEvent::Context {
            estimated: i as usize,
        })
        .unwrap();
        if i % 100 == 0 {
            let summary = Usage {
                input: Some(i),
                output: None,
                cached: Some(0),
                ..Usage::default()
            };
            expected_apec.record_batch(2, Some(&summary));
            tx.send(UiEvent::ApecUsage {
                requests: 2,
                input: summary.input,
                output: summary.output,
                cached: summary.cached,
            })
            .unwrap();
        }
    }
    tx.send(UiEvent::ActualInput(42)).unwrap();
    tx.send(UiEvent::TaskFinished).unwrap();
    let events = drain(&mut rx);
    assert!(
        events.len() <= 7,
        "accounting must not retain one record per request"
    );
    assert!(matches!(events.last(), Some(UiEvent::TaskFinished)));
    assert!(events.iter().any(|e| matches!(e, UiEvent::ActualInput(42))));
    let mut coding = UsageMeter::default();
    let mut apec = UsageMeter::default();
    for event in events {
        match event {
            UiEvent::UsageBatch(batch) => coding.absorb(&batch),
            UiEvent::ApecUsageBatch(batch) => apec.absorb(&batch),
            _ => {}
        }
    }
    assert_eq!(
        serde_json::to_value(coding).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        serde_json::to_value(apec).unwrap(),
        serde_json::to_value(expected_apec).unwrap()
    );
    tx.send(UiEvent::TaskStarted).unwrap();
    tx.send(UiEvent::Text("下一轮".into())).unwrap();
    assert!(matches!(rx.try_recv(), Ok(UiEvent::TaskStarted)));
    assert!(matches!(rx.try_recv(), Ok(UiEvent::Text(s)) if s == "下一轮"));
}

#[tokio::test(flavor = "current_thread")]
async fn title_accounting_waits_without_blocking_and_keeps_originating_ids() {
    let (tx, mut rx) = channel();
    tx.send(UiEvent::Text("x".repeat(MAX_BYTES))).unwrap();
    for i in 0..TITLE_EVENTS {
        tx.send(UiEvent::TitleUsage {
            session_id: format!("session-{i}"),
            requests: 1,
            input: Some(i as u64),
            output: None,
            cached: None,
            reasoning: None,
        })
        .unwrap();
    }
    let sender = tx.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _permit = sender.title_permit("last-session").unwrap();
        started.send(()).unwrap();
        sender
            .send_title(UiEvent::TitleUsage {
                session_id: "last-session".into(),
                requests: 2,
                input: Some(99),
                output: Some(7),
                cached: None,
                reasoning: None,
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();
    assert!(
        !task.is_finished(),
        "a full reliable lane must yield to the consumer"
    );
    for i in 0..TITLE_EVENTS {
        assert!(
            matches!(rx.try_recv(), Ok(UiEvent::TitleUsage { session_id, input: Some(input), .. })
            if session_id == format!("session-{i}") && input == i as u64)
        );
    }
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(rx.try_recv(), Ok(UiEvent::TitleUsage { session_id, requests: 2, .. }) if session_id == "last-session")
    );
    assert!(matches!(rx.try_recv(), Ok(UiEvent::BacklogOverflow)));
}

#[test]
fn title_work_and_identity_are_bounded_before_starting_a_request() {
    let (tx, _rx) = channel();
    let mut permits = (0..TITLE_TASKS)
        .map(|_| tx.title_permit("session").unwrap())
        .collect::<Vec<_>>();
    assert!(tx.title_permit("another-session").is_none());
    permits.pop();
    assert!(tx.title_permit("another-session").is_some());
    assert!(tx.title_permit(&"s".repeat(MAX_SESSION_ID + 1)).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_receive_does_not_consume_a_later_wakeup() {
    let (tx, mut rx) = channel();
    assert!(rx.recv().now_or_never().is_none());
    tx.send(UiEvent::Text("取消等待后".into())).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap();
    assert!(matches!(result, Some(UiEvent::Text(s)) if s == "取消等待后"));
    let thread = std::thread::spawn(move || {
        tx.send(UiEvent::Text("最后一条".into())).unwrap();
    });
    let result = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap();
    assert!(matches!(result, Some(UiEvent::Text(s)) if s == "最后一条"));
    thread.join().unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_drop_releases_retained_output_and_wakes_blocked_title_senders() {
    let (tx, rx) = channel();
    tx.send(UiEvent::Text("retained".into())).unwrap();
    for _ in 0..TITLE_EVENTS {
        tx.send(UiEvent::SessionTitleUpdated("session".into()))
            .unwrap();
    }
    let sender = tx.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        started.send(()).unwrap();
        sender
            .send_title(UiEvent::SessionTitleUpdated("waiting".into()))
            .await
    });
    ready.await.unwrap();
    assert!(!task.is_finished());
    drop(rx);
    assert!(tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(tx.send(UiEvent::TaskStarted).is_err());
    assert!(tx.title_permit("session").is_none());
    assert_eq!(tx.shared.state.lock().unwrap().bytes, 0);
}

#[test]
fn late_git_status_still_arrives_after_presentation_was_paused() {
    let (tx, mut rx) = channel();
    tx.send(UiEvent::Text("x".repeat(MAX_BYTES))).unwrap();
    drain(&mut rx);
    tx.send(UiEvent::TaskFinished).unwrap();
    for revision in 1..=100 {
        tx.send(UiEvent::GitInfo {
            revision,
            session: "same-session".into(),
            info: super::super::gitinfo::Info {
                branch: Some("完整分支🦀".into()),
                dirty: true,
            },
            changes: None,
        })
        .unwrap();
    }
    assert!(matches!(rx.try_recv(), Ok(UiEvent::TaskFinished)));
    assert!(
        matches!(rx.try_recv(), Ok(UiEvent::GitInfo { revision: 100, session, info, .. })
        if session == "same-session" && info.branch.as_deref() == Some("完整分支🦀"))
    );
    let state = tx.shared.state.lock().unwrap();
    assert_eq!(state.bytes, 0);
}
