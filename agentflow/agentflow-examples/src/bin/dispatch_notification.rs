//! Dispatch notification tasks using AgentFlow message bus
//! This demonstrates how to use the AgentFlow framework programmatically

use std::sync::Arc;

use agentflow_core::{AgentMessage, bus::{MessageBus, InMemoryBus}};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 Starting AgentFlow notification test dispatch...\n");

    // Create message bus
    let bus = Arc::new(InMemoryBus::new(100));
    let bus_sender = bus.clone();

    println!("✅ AgentFlow in-memory message bus created");
    println!();

    // Send GitHub Status message
    println!("📋 Sending PostGitHubStatus message...");
    let message1 = AgentMessage::PostGitHubStatus {
        owner: "tobias-weiss-ai-xr".to_string(),
        repo: "argunix".to_string(),
        sha: "abc123def456".to_string(),
        state: Some("success".to_string()),
        description: Some("Test notification from AgentFlow".to_string()),
        target_url: Some("https://ci.opendesk.works/builds/123".to_string()),
        task_id: None,
    };

    <dyn MessageBus>::send(&*bus_sender, message1).await?;
    println!("✅ PostGitHubStatus message sent");
    println!("   owner: tobias-weiss-ai-xr");
    println!("   repo: argunix");
    println!("   sha: abc123def456");
    println!("   state: success");
    println!("   url: https://ci.opendesk.works/builds/123");
    println!();

    // Send UpdateGitHubStatus message
    println!("📋 Sending UpdateGitHubStatus message...");
    let message2 = AgentMessage::UpdateGitHubStatus {
        owner: "tobias-weiss-ai-xr".to_string(),
        repo: "argunix".to_string(),
        sha: "abc123def456".to_string(),
        state: Some("failure".to_string()),
        description: Some("CI build failed".to_string()),
        task_id: None,
    };

    <dyn MessageBus>::send(&*bus_sender, message2).await?;
    println!("✅ UpdateGitHubStatus message sent");
    println!("   owner: tobias-weiss-ai-xr");
    println!("   repo: argunix");
    println!("   sha: abc123def456");
    println!("   state: failure");
    println!();

    // Send Matrix Notification message
    println!("📋 Sending SendMatrixNotification message...");
    let message3 = AgentMessage::SendMatrixNotification {
        room: "!test:matrix.org".to_string(),
        message: "Hello from AgentFlow! 🎉".to_string(),
        formatted: Some("<b>Hello from AgentFlow! 🎉</b>".to_string()),
        task_id: None,
    };

    <dyn MessageBus>::send(&*bus_sender, message3).await?;
    println!("✅ SendMatrixNotification message sent");
    println!("   room: !test:matrix.org");
    println!("   message: Hello from AgentFlow! 🎉");
    println!("   formatted: <b>Hello from AgentFlow! 🎉</b>");
    println!();

    // Send Matrix Broadcast message
    println!("📋 Sending BroadcastMatrixMessage message...");
    let message4 = AgentMessage::BroadcastMatrixMessage {
        message: "Broadcast test from AgentFlow".to_string(),
        rooms: vec![
            "!builds:matrix.org".to_string(),
            "!alerts:matrix.org".to_string(),
        ],
        task_id: None,
    };

    <dyn MessageBus>::send(&*bus_sender, message4).await?;
    println!("✅ BroadcastMatrixMessage message sent");
    println!("   rooms: [!builds:matrix.org, !alerts:matrix.org]");
    println!("   message: Broadcast test from AgentFlow");
    println!();

    // Send Matrix File message
    println!("📋 Sending SendMatrixFile message...");
    let message5 = AgentMessage::SendMatrixFile {
        file_name: "test.txt".to_string(),
        content_type: Some("text/plain".to_string()),
        data: b"Hello from AgentFlow file!".to_vec(),
        room: Some("!files:matrix.org".to_string()),
        task_id: None,
    };

    <dyn MessageBus>::send(&*bus_sender, message5).await?;
    println!("✅ SendMatrixFile message sent");
    println!("   file_name: test.txt");
    println!("   content_type: text/plain");
    println!("   data: 22 bytes");
    println!("   room: !files:matrix.org");
    println!();

    println!("🎉 All notification messages dispatched successfully!");
    println!();
    println!("📝 Next steps:");
    println!("   To see actual processing, start the agents:");
    println!("   - GitHubStatusAgent: handles PostGitHubStatus, UpdateGitHubStatus");
    println!("   - MatrixNotifierAgent: handles SendMatrixNotification, BroadcastMatrixMessage, SendMatrixFile");
    println!();
    println!("   Use the following to run:");
    println!("   cargo run --package agentflow-server");

    Ok(())
}
