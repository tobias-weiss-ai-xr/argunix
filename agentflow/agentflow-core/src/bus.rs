//! Message Bus - Abstraction for inter-agent communication

use crate::message::AgentMessage;
use crate::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Message bus receiver
pub struct MessageStream {
    pub receiver: mpsc::Receiver<AgentMessage>,
}

impl MessageStream {
    /// Create a new message stream from a receiver
    pub fn new(receiver: mpsc::Receiver<AgentMessage>) -> Self {
        Self { receiver }
    }
    
    /// Receive the next message
    pub async fn next(&mut self) -> Option<AgentMessage> {
        self.receiver.recv().await
    }
}

/// Trait for message bus implementations
#[async_trait]
pub trait MessageBus: Send + Sync {
    /// Send a message to a specific agent or topic
    async fn send(&self, message: AgentMessage) -> Result<()>;
    
    /// Send a message to a specific topic/subject
    async fn send_to(&self, topic: &str, message: AgentMessage) -> Result<()>;
    
    /// Subscribe to a topic and get a message stream
    async fn subscribe(&self, topic: &str) -> Result<MessageStream>;
    
    /// Create a new message bus instance
    fn clone_bus(&self) -> Box<dyn MessageBus>;
}

/// In-memory message bus implementation (for testing and single-node mode)
pub struct InMemoryBus {
    sender: mpsc::Sender<AgentMessage>,
    receiver: Arc<tokio::sync::Mutex<mpsc::Receiver<AgentMessage>>>,
}

impl InMemoryBus {
    /// Create a new in-memory bus
    pub fn new(buffer: usize) -> Self {
        let (sender, receiver) = mpsc::channel(buffer);
        
        Self {
            sender,
            receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
        }
    }
}

#[async_trait]
impl MessageBus for InMemoryBus {
    async fn send(&self, message: AgentMessage) -> Result<()> {
        self.sender.send(message).await
            .map_err(|e| crate::AgentFlowError::ChannelSend(e))?;
        Ok(())
    }
    
    async fn send_to(&self, _topic: &str, message: AgentMessage) -> Result<()> {
        // In in-memory mode, topics are ignored
        self.send(message).await
    }
    
    async fn subscribe(&self, _topic: &str) -> Result<MessageStream> {
        // In this simple implementation, we create a new channel and spawn a task
        // to forward messages from the main receiver
        let (forward_sender, forward_receiver) = mpsc::channel(1000);
        let receiver = self.receiver.clone();
        
        tokio::spawn(async move {
            let mut inner_receiver = receiver.lock().await;
            while let Some(msg) = inner_receiver.recv().await {
                let _ = forward_sender.send(msg).await;
            }
        });
        
        Ok(MessageStream::new(forward_receiver))
    }
    
    fn clone_bus(&self) -> Box<dyn MessageBus> {
        Box::new(Self {
            sender: self.sender.clone(),
            receiver: self.receiver.clone(),
        })
    }
}

impl Clone for InMemoryBus {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            receiver: self.receiver.clone(),
        }
    }
}

/// NATS-based message bus implementation for distributed mode
#[cfg(feature = "nats")]
pub struct NatsBus {
    client: async_nats::Client,
    pub_prefix: String,
}

#[cfg(feature = "nats")]
impl NatsBus {
    /// Create a new NATS message bus
    pub async fn new(nats_url: &str, pub_prefix: &str) -> Result<Self> {
        let client = async_nats::connect(nats_url)
            .await
            .map_err(|e| crate::AgentFlowError::Network(e.to_string()))?;
        
        Ok(Self {
            client,
            pub_prefix: pub_prefix.to_string(),
        })
    }
    
    /// Get the subject for a message type
    pub fn subject_for_message(&self, message: &AgentMessage) -> String {
        match message {
            AgentMessage::SubmitTask(_) => format!("{}.tasks.submit", self.pub_prefix),
            AgentMessage::ExecuteTask(_) => format!("{}.tasks.execute", self.pub_prefix),
            AgentMessage::TaskResult(_) => format!("{}.tasks.result", self.pub_prefix),
            AgentMessage::TaskFailed { .. } => format!("{}.tasks.failed", self.pub_prefix),
            AgentMessage::TaskScheduled { .. } => format!("{}.tasks.scheduled", self.pub_prefix),
            AgentMessage::FlakeAnalysisComplete { .. } => format!("{}.flakes.analysis", self.pub_prefix),
            AgentMessage::AnalyzeFlake { .. } => format!("{}.flakes.analyze", self.pub_prefix),
            AgentMessage::EvaluateFlake { .. } => format!("{}.flakes.evaluate", self.pub_prefix),
            AgentMessage::AgentReady { .. } => format!("{}.agents.ready", self.pub_prefix),
            AgentMessage::AgentBusy { .. } => format!("{}.agents.busy", self.pub_prefix),
            AgentMessage::AgentIdle { .. } => format!("{}.agents.idle", self.pub_prefix),
            AgentMessage::RegisterAgent(_) => format!("{}.agents.register", self.pub_prefix),
            AgentMessage::DeregisterAgent { .. } => format!("{}.agents.deregister", self.pub_prefix),
            AgentMessage::Heartbeat { .. } => format!("{}.agents.heartbeat", self.pub_prefix),
            AgentMessage::Log { .. } => format!("{}.logs", self.pub_prefix),
            AgentMessage::CancelTask { .. } => format!("{}.tasks.cancel", self.pub_prefix),
            AgentMessage::BuildDrv { .. } => format!("{}.builds.drv", self.pub_prefix),
            _ => format!("{}.general", self.pub_prefix),
        }
    }
}

#[cfg(feature = "nats")]
#[async_trait]
impl MessageBus for NatsBus {
    async fn send(&self, message: AgentMessage) -> Result<()> {
        let subject = self.subject_for_message(&message);
        let bytes = bincode::serialize(&message)
            .map_err(|e| crate::AgentFlowError::Generic(e.to_string()))?;
        
        self.client.publish(subject, bytes.into()).await
            .map_err(|e| crate::AgentFlowError::Network(e.to_string()))?;
        
        Ok(())
    }
    
    async fn send_to(&self, topic: &str, message: AgentMessage) -> Result<()> {
        let full_subject = format!("{}.{}", self.pub_prefix, topic);
        let bytes = bincode::serialize(&message)
            .map_err(|e| crate::AgentFlowError::Generic(e.to_string()))?;
        
        self.client.publish(full_subject, bytes.into()).await
            .map_err(|e| crate::AgentFlowError::Network(e.to_string()))?;
        
        Ok(())
    }
    
    async fn subscribe(&self, topic: &str) -> Result<MessageStream> {
        // For now, return an empty stream as the full NATS implementation
        // requires async-nats v0.32 which has a different API.
        // This stub allows the code to compile with the feature enabled.
        // TODO: Implement full NATS subscription when async-nats API is stable.
        let (_sender, receiver) = mpsc::channel(1000);
        Ok(MessageStream::new(receiver))
    }
    
    fn clone_bus(&self) -> Box<dyn MessageBus> {
        Box::new(Self {
            client: self.client.clone(),
            pub_prefix: self.pub_prefix.clone(),
        })
    }
}

#[cfg(feature = "nats")]
impl Clone for NatsBus {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            pub_prefix: self.pub_prefix.clone(),
        }
    }
}

/// Message bus type aliase
pub type BusSender = mpsc::Sender<AgentMessage>;
pub type BusReceiver = mpsc::Receiver<AgentMessage>;

/// Create a simple in-memory message bus for testing
pub fn create_in_memory_bus(buffer: usize) -> Arc<dyn MessageBus> {
    let (sender, receiver) = mpsc::channel(buffer);
    
    Arc::new(InMemoryBus {
        sender: sender.clone(),
        receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
    })
}

/// Trait for creating message bus instances
pub trait MessageBusFactory: Send + Sync {
    fn create_bus(&self) -> Box<dyn MessageBus>;
}

/// In-memory message bus factory
pub struct InMemoryBusFactory;

impl MessageBusFactory for InMemoryBusFactory {
    fn create_bus(&self) -> Box<dyn MessageBus> {
        let in_memory_bus = InMemoryBus::new(10000);
        Box::new(in_memory_bus)
    }
}

/// NATS message bus factory
#[cfg(feature = "nats")]
pub struct NatsBusFactory {
    nats_url: String,
    pub_prefix: String,
}

#[cfg(feature = "nats")]
impl NatsBusFactory {
    pub fn new(nats_url: String, pub_prefix: String) -> Self {
        Self { nats_url, pub_prefix }
    }
}

#[cfg(feature = "nats")]
#[async_trait]
impl MessageBusFactory for NatsBusFactory {
    fn create_bus(&self) -> Box<dyn MessageBus> {
        // This is async but we're in a sync trait - need to handle this differently
        // For now, we'll block on the async call
        // In practice, this should be created at startup
        todo!("Async bus factory not yet implemented")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_in_memory_bus() {
        let bus = create_in_memory_bus(100);
        
        // Create a sender by downcasting the bus to InMemoryBus
        // This is a bit of a hack but works for testing
        
        // Send a message
        let message = AgentMessage::Log {
            level: "info".to_string(),
            message: "Test message".to_string(),
            agent_id: None,
            task_id: None,
        };
        
        // Send via the bus
        bus.send(message.clone()).await.unwrap();
        
        // Subscribe and receive
        let mut stream = bus.subscribe("test").await.unwrap();
        let received = stream.next().await;
        assert!(received.is_some());
        assert!(matches!(received.unwrap(), AgentMessage::Log { .. }));
    }
}
