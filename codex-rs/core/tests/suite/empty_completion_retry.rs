//! A sampling request that returns an empty assistant message with no tool call
//! used to end the turn silently: nothing was wrong, there was simply nothing in
//! it. Local models on an unconstrained decoder do this regularly, and the user
//! saw a reprinted preamble and a dead turn. The turn loop now resamples once.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use wiremock::MockServer;

fn empty_completion(id: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message(&format!("msg-{id}"), ""),
        ev_completed(id),
    ])
}

fn text_completion(id: &str, text: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message(&format!("msg-{id}"), text),
        ev_completed(id),
    ])
}

/// Drives one turn to completion, returning every event it emitted.
async fn run_turn(server: &MockServer, prompt: &str) -> Result<Vec<EventMsg>> {
    let mut builder = test_codex();
    let test = builder.build_with_auto_env(server).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut events = Vec::new();
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        let done = matches!(event, EventMsg::TurnComplete(_));
        events.push(event);
        if done {
            break;
        }
    }
    Ok(events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_completion_is_resampled_and_the_turn_recovers() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            empty_completion("resp-1"),
            text_completion("resp-2", "here is the answer"),
        ],
    )
    .await;

    let events = run_turn(&server, "why did the last album test duplicate?").await?;

    assert_eq!(
        responses.requests().len(),
        2,
        "the empty completion must be resampled rather than ending the turn"
    );
    let answered = events.iter().any(|event| {
        matches!(event, EventMsg::AgentMessage(message) if message.message.contains("here is the answer"))
    });
    assert!(answered, "the resampled completion must reach the user");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_empty_completion_ends_the_turn_with_a_warning() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let responses = mount_sse_sequence(
        &server,
        vec![empty_completion("resp-1"), empty_completion("resp-2")],
    )
    .await;

    let events = run_turn(&server, "why did the last album test duplicate?").await?;

    assert_eq!(
        responses.requests().len(),
        2,
        "the retry is capped at one; a stalling model must not be resampled forever"
    );
    let warned = events
        .iter()
        .any(|event| matches!(event, EventMsg::Warning(_)));
    assert!(
        warned,
        "a turn that ends with no answer must say so instead of completing in silence"
    );
    Ok(())
}
