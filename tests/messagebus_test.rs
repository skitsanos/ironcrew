use ironcrew::engine::messagebus::{Message, MessageBus, MessageType};

#[tokio::test]
async fn test_send_and_receive() {
    let bus = MessageBus::new();
    bus.register_agent("alice").await;
    bus.register_agent("bob").await;

    let msg = Message::new(
        "alice".into(),
        "bob".into(),
        "hello bob".into(),
        MessageType::Notification,
    );
    bus.send(msg).await;

    let received = bus.receive("bob").await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].content, "hello bob");
    assert_eq!(received[0].from, "alice");

    // Should be empty after consuming
    let received2 = bus.receive("bob").await;
    assert!(received2.is_empty());
}

#[tokio::test]
async fn test_broadcast() {
    let bus = MessageBus::new();
    bus.register_agent("alice").await;
    bus.register_agent("bob").await;
    bus.register_agent("charlie").await;

    let msg = Message::new(
        "alice".into(),
        "*".into(),
        "hello everyone".into(),
        MessageType::Broadcast,
    );
    bus.send(msg).await;

    // Bob and Charlie should get it, but not Alice (the sender)
    assert_eq!(bus.receive("bob").await.len(), 1);
    assert_eq!(bus.receive("charlie").await.len(), 1);
    assert_eq!(bus.receive("alice").await.len(), 0);
}

#[tokio::test]
async fn test_reply() {
    let bus = MessageBus::new();
    bus.register_agent("alice").await;
    bus.register_agent("bob").await;

    let msg = Message::new(
        "alice".into(),
        "bob".into(),
        "what's the status?".into(),
        MessageType::Request,
    );
    let msg_id = msg.id.clone();
    bus.send(msg).await;

    let received = bus.receive("bob").await;
    let reply = Message::reply(&received[0], "bob".into(), "all good".into());
    assert_eq!(reply.reply_to, Some(msg_id));
    bus.send(reply).await;

    let alice_msgs = bus.receive("alice").await;
    assert_eq!(alice_msgs.len(), 1);
    assert_eq!(alice_msgs[0].content, "all good");
}

#[tokio::test]
async fn test_pending_count() {
    let bus = MessageBus::new();
    bus.register_agent("bob").await;

    assert_eq!(bus.pending_count("bob").await, 0);

    bus.send(Message::new(
        "a".into(),
        "bob".into(),
        "1".into(),
        MessageType::Notification,
    ))
    .await;
    bus.send(Message::new(
        "a".into(),
        "bob".into(),
        "2".into(),
        MessageType::Notification,
    ))
    .await;

    assert_eq!(bus.pending_count("bob").await, 2);
}

#[tokio::test]
async fn test_history() {
    let bus = MessageBus::new();
    bus.register_agent("alice").await;

    bus.send(Message::new(
        "alice".into(),
        "bob".into(),
        "msg1".into(),
        MessageType::Notification,
    ))
    .await;
    bus.send(Message::new(
        "alice".into(),
        "bob".into(),
        "msg2".into(),
        MessageType::Notification,
    ))
    .await;

    let history = bus.get_history().await;
    assert_eq!(history.len(), 2);
}

#[tokio::test]
async fn test_receive_empty() {
    let bus = MessageBus::new();
    let msgs = bus.receive("nobody").await;
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn test_peek_does_not_consume() {
    let bus = MessageBus::new();
    bus.register_agent("bob").await;

    bus.send(Message::new(
        "a".into(),
        "bob".into(),
        "peek me".into(),
        MessageType::Notification,
    ))
    .await;

    let peeked = bus.peek("bob").await;
    assert_eq!(peeked.len(), 1);

    // Should still be there after peek
    let peeked2 = bus.peek("bob").await;
    assert_eq!(peeked2.len(), 1);

    // Consume now
    let received = bus.receive("bob").await;
    assert_eq!(received.len(), 1);

    // Now empty
    assert_eq!(bus.pending_count("bob").await, 0);
}
