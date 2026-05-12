/// Integration test with real OpenAI API.
///
/// Usage:
/// 1. Copy `.env.example` to `.env`
/// 2. Fill in your `OPENAI_API_KEY` and optionally `OPENAI_BASE_URL`
/// 3. Run: `cargo test --test integration_openai -- --ignored`

use std::time::Duration;

use funcode::{Agent, BashTool, Bus, Event, Model, OpenAIProvider, ReceiveResult, Session, ToolRegistry};

#[tokio::test]
#[ignore] // Run with: cargo test --test integration_openai -- --ignored
async fn test_openai_real_flow() {
    // Load .env file
    dotenv::dotenv().expect("Failed to load .env file");

    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in .env");
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    println!("=== OpenAI Integration Test ===");
    println!("Model: {}", model_name);
    println!("Base URL: {:?}", base_url.as_deref().unwrap_or("default"));
    println!();

    // Build provider
    let provider = OpenAIProvider::new(api_key, base_url)
        .expect("Failed to create OpenAI provider");

    // Build model
    let model = Model::new(Box::new(provider), &model_name)
        .expect("Failed to create Model");

    // Build session with a simple system prompt
    let session = Session::new(
        "You are a helpful AI assistant. Answer concisely.",
        100_000,
    );

    // Build empty tool registry (no tools for this test)
    let registry = ToolRegistry::new();

    // Build agent
    let agent = Agent::new(model, session, registry, Bus::new(64), 10);

    // Spawn agent with handle
    let handle = agent.spawn(16);
    let mut subscriber = handle.subscribe();

    // Subscribe to events in background
    let event_task = tokio::spawn(async move {
        let mut text_output = String::new();
        let mut turn_complete = false;
        loop {
            // Exit if turn completed and no more events pending
            if turn_complete {
                // Small timeout to catch any remaining events
                match tokio::time::timeout(Duration::from_millis(100), subscriber.recv()).await {
                    Ok(Some(_)) => continue, // Process remaining event
                    _ => break,               // Timeout or closed -> exit
                }
            }

            match subscriber.recv().await {
                Some(ReceiveResult::Event(event)) => {
                    match &event {
                        Event::TurnStarted => {
                            print!("\n[Turn started]\n> ");
                        }
                        Event::TextDelta(delta) => {
                            print!("{}", delta);
                            text_output.push_str(delta);
                        }
                        Event::TextDone(text) => {
                            println!("\n[Text done: {} chars]", text.len());
                        }
                        Event::TurnComplete { usage } => {
                            if let Some(u) = usage {
                                println!(
                                    "\n[Turn complete: input={} output={} total={}]",
                                    u.input_tokens.unwrap_or(0),
                                    u.output_tokens.unwrap_or(0),
                                    u.total_tokens.unwrap_or(0)
                                );
                            }
                            turn_complete = true;
                        }
                        Event::TurnInterrupted => {
                            println!("\n[Turn interrupted]");
                            break;
                        }
                        Event::Error(err) => {
                            println!("\n[Error: {}]", err);
                            break;
                        }
                        _ => {
                            // Other events not printed in this simple test
                        }
                    }
                }
                Some(ReceiveResult::Lagged(n)) => {
                    println!("\n[Lagged: {} events dropped]", n);
                    break;
                }
                None => {
                    println!("\n[Channel closed]");
                    break;
                }
            }
        }
        text_output
    });

    // Send user turn
    let user_input = "What is 2+2? Answer with just the number.";
    println!("Sending: {}", user_input);

    handle
        .user_turn(user_input.to_string())
        .await
        .expect("Failed to send user turn");

    // Wait for events to be processed
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Shutdown
    println!("\nShutting down...");
    handle.shutdown().await.expect("Failed to shutdown");

    // Get the final output
    let text_output = tokio::time::timeout(Duration::from_secs(2), event_task)
        .await
        .expect("Event task timeout")
        .expect("Event task join error");

    println!("\n=== Test Complete ===");
    println!("Total output length: {} chars", text_output.len());
    println!("Output preview: {}", text_output.chars().take(100).collect::<String>());
}

#[tokio::test]
#[ignore] // Run with: cargo test --test integration_openai -- --ignored
async fn test_openai_tool_use() {
    // Load .env file
    dotenv::dotenv().expect("Failed to load .env file");

    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in .env");
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    println!("=== OpenAI Tool Use Integration Test ===");
    println!("Model: {}", model_name);
    println!("Base URL: {:?}", base_url.as_deref().unwrap_or("default"));
    println!();

    // Build provider
    let provider = OpenAIProvider::new(api_key, base_url)
        .expect("Failed to create OpenAI provider");

    // Build model
    let model = Model::new(Box::new(provider), &model_name)
        .expect("Failed to create Model");

    // Build session with a system prompt that encourages tool use
    let session = Session::new(
        "You are a helpful AI assistant. When the user asks you to execute a command, use the Bash tool.",
        100_000,
    );

    // Build tool registry with BashTool
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(BashTool::new()));

    // Build agent
    let agent = Agent::new(model, session, registry, Bus::new(64), 10);

    // Spawn agent with handle
    let handle = agent.spawn(16);
    let mut subscriber = handle.subscribe();

    // Track tool execution
    let tool_call_start = std::sync::Arc::new(std::sync::Mutex::new(false));
    let tool_call_end = std::sync::Arc::new(std::sync::Mutex::new(false));
    let tool_output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

    // Subscribe to events in background
    let event_task = tokio::spawn({
        let tool_call_start = tool_call_start.clone();
        let tool_call_end = tool_call_end.clone();
        let tool_output = tool_output.clone();
        async move {
            let mut text_output = String::new();
            let mut turn_complete = false;
            loop {
                // Exit if turn completed
                if turn_complete {
                    match tokio::time::timeout(Duration::from_millis(100), subscriber.recv()).await {
                        Ok(Some(_)) => continue,
                        _ => break,
                    }
                }

                match subscriber.recv().await {
                    Some(ReceiveResult::Event(event)) => {
                        match &event {
                            Event::TurnStarted => {
                                println!("\n[Turn started]");
                            }
                            Event::TextDelta(delta) => {
                                print!("{}", delta);
                                text_output.push_str(delta);
                            }
                            Event::TextDone(text) => {
                                if !text.is_empty() {
                                    println!("\n[Text done: {} chars]", text.len());
                                }
                            }
                            Event::ToolCallBegin { id, name } => {
                                println!("\n[Tool call start: {} ({})]", name, id);
                                *tool_call_start.lock().unwrap() = true;
                            }
                            Event::ToolCallEnd { id, name, output, is_error } => {
                                println!(
                                    "\n[Tool call end: {} ({}) - error: {}]",
                                    name, id, is_error
                                );
                                println!("[Tool output: {}]", output);
                                *tool_call_end.lock().unwrap() = true;
                                *tool_output.lock().unwrap() = output.clone();
                            }
                            Event::TurnComplete { usage } => {
                                if let Some(u) = usage {
                                    println!(
                                        "\n[Turn complete: input={} output={} total={}]",
                                        u.input_tokens.unwrap_or(0),
                                        u.output_tokens.unwrap_or(0),
                                        u.total_tokens.unwrap_or(0)
                                    );
                                }
                                turn_complete = true;
                            }
                            Event::TurnInterrupted => {
                                println!("\n[Turn interrupted]");
                                break;
                            }
                            Event::Error(err) => {
                                println!("\n[Error: {}]", err);
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(ReceiveResult::Lagged(n)) => {
                        println!("\n[Lagged: {} events dropped]", n);
                        break;
                    }
                    None => {
                        println!("\n[Channel closed]");
                        break;
                    }
                }
            }
            (text_output, tool_call_start, tool_call_end, tool_output)
        }
    });

    // Send user turn asking to use bash tool
    let user_input = "Execute this bash command: echo 'hello world'";
    println!("Sending: {}", user_input);

    handle
        .user_turn(user_input.to_string())
        .await
        .expect("Failed to send user turn");

    // Wait for events to be processed
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Shutdown
    println!("\nShutting down...");
    handle.shutdown().await.expect("Failed to shutdown");

    // Get the results
    let (_text_output, tool_call_start, tool_call_end, tool_output) =
        tokio::time::timeout(Duration::from_secs(2), event_task)
            .await
            .expect("Event task timeout")
            .expect("Event task join error");

    println!("\n=== Test Results ===");
    println!("Tool call started: {}", *tool_call_start.lock().unwrap());
    println!("Tool call ended: {}", *tool_call_end.lock().unwrap());
    println!("Tool output: {}", *tool_output.lock().unwrap());

    // Assertions
    assert!(
        *tool_call_start.lock().unwrap(),
        "Expected ToolCallBegin event"
    );
    assert!(
        *tool_call_end.lock().unwrap(),
        "Expected ToolCallEnd event"
    );

    let output = tool_output.lock().unwrap();
    assert!(
        output.contains("hello world"),
        "Expected output to contain 'hello world', got: {}",
        output
    );

    println!("\n=== Tool Use Test Passed! ===");
}
