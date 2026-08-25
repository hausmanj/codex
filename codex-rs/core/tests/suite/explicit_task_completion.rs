use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_using_turn_resamples_until_completion_is_declared() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "plan-call",
                    "update_plan",
                    &json!({
                        "plan": [{"step": "inspect", "status": "completed"}]
                    })
                    .to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("premature-message", "I will do one last check."),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_function_call(
                    "complete-call",
                    "complete_task",
                    &json!({
                        "status": "completed",
                        "final_message": "The requested work is complete."
                    })
                    .to_string(),
                ),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            let _ = config.features.enable(Feature::ExplicitTaskCompletion);
        })
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect the project and finish the work.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let EventMsg::TurnComplete(completed) = wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!("event predicate only accepts TurnComplete");
    };

    assert_eq!(
        completed.last_agent_message.as_deref(),
        Some("The requested work is complete.")
    );
    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0].body_json()["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "complete_task"))
    );
    assert!(
        requests[2]
            .message_input_texts("developer")
            .iter()
            .any(|text| text.contains("<task_completion_required>")),
        "the retry must explain the structured completion contract"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_answer_resamples_until_completion_is_declared() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("answer", "A direct answer."),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call(
                    "complete-call",
                    "complete_task",
                    &json!({
                        "status": "completed",
                        "final_message": "A direct answer."
                    })
                    .to_string(),
                ),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            let _ = config.features.enable(Feature::ExplicitTaskCompletion);
        })
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Answer directly.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let EventMsg::TurnComplete(completed) = wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!("event predicate only accepts TurnComplete");
    };

    assert_eq!(
        completed.last_agent_message.as_deref(),
        Some("A direct answer.")
    );
    assert_eq!(responses.requests().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn productive_tools_reset_the_missing_completion_retry_budget() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut sequence = Vec::new();
    for step in 1..=4 {
        sequence.push(sse(vec![
            ev_response_created(&format!("message-{step}")),
            ev_assistant_message(
                &format!("message-item-{step}"),
                &format!("Continuing with step {step}."),
            ),
            ev_completed(&format!("message-{step}")),
        ]));
        sequence.push(sse(vec![
            ev_response_created(&format!("tool-{step}")),
            ev_function_call(
                &format!("plan-call-{step}"),
                "update_plan",
                &json!({
                    "plan": [{"step": format!("step {step}"), "status": "completed"}]
                })
                .to_string(),
            ),
            ev_completed(&format!("tool-{step}")),
        ]));
    }
    sequence.push(sse(vec![
        ev_response_created("complete"),
        ev_function_call(
            "complete-call",
            "complete_task",
            &json!({
                "status": "completed",
                "final_message": "All four productive phases completed."
            })
            .to_string(),
        ),
        ev_completed("complete"),
    ]));

    let responses = mount_sse_sequence(&server, sequence).await;
    let test = test_codex()
        .with_config(|config| {
            let _ = config.features.enable(Feature::ExplicitTaskCompletion);
        })
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Complete four work phases.".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let EventMsg::TurnComplete(completed) = wait_for_event(test.codex.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!("event predicate only accepts TurnComplete");
    };

    assert_eq!(
        completed.last_agent_message.as_deref(),
        Some("All four productive phases completed.")
    );
    assert_eq!(responses.requests().len(), 9);
    Ok(())
}
