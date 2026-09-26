use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use scrapix_queue::{AnyConsumer, AnyProducer, ChannelBus};

#[tokio::test]
async fn process_with_ack_delivers_every_message_with_an_ack() {
    let bus = ChannelBus::new();
    let producer = AnyProducer::channel(bus.producer());
    let consumer = AnyConsumer::channel(bus.consumer());
    consumer.subscribe(&["t"]).unwrap();
    for i in 0..20u32 {
        producer.send("t", None, &i).await.unwrap();
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (s, sd) = (seen.clone(), shutdown.clone());
    let run = tokio::spawn(async move {
        consumer
            .process_with_ack::<u32, _, _>(
                move |_n, _meta, ack| {
                    let s = s.clone();
                    async move {
                        s.fetch_add(1, Ordering::SeqCst);
                        ack.ack();
                    }
                },
                4,
                sd,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while seen.load(Ordering::SeqCst) < 20 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all messages delivered");
    shutdown.store(true, Ordering::SeqCst);
    run.await.unwrap().unwrap();
}
