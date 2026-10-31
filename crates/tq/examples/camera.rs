//! A camera and a channel, published in about ten lines. H.264 or H.265
//! comes in on stdin; messages on `lights` are printed and acked.
//!
//! ```sh
//! dev/run-local.sh   # a dev relay on :8080, secret dev-secret-change-me
//! ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 -c:v libx264 \
//!     -tune zerolatency -g 30 -f h264 - \
//!   | TQ_SHARED_SECRET=dev-secret-change-me cargo run --release -p tq --example camera
//! ```
//!
//! Then open `http://localhost:8080/?room=robot-01&preset=operator`, or
//! `tq-probe --room robot-01 --preset operator --send 'rover-7/lights={"on":true}'`.

use std::io::Read;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let (rover, mut inbox) = tq::Publication::builder("rover-7").video("front").channel("lights").build()?;
    let token = tq::Token::publisher(std::env::var("TQ_SHARED_SECRET")?, "rover-7");
    let client = tq::Client::discover("http://localhost:8080", "robot-01", token)?;
    client.publish(&rover, Default::default());

    let mut front = rover.video("front").unwrap();
    std::thread::spawn(move || {
        let mut buf = vec![0; 64 << 10];
        while let Ok(n @ 1..) = std::io::stdin().read(&mut buf) {
            let _ = front.write(&buf[..n]);
        }
        front.reset("stdin closed");
    });

    while let Some(msg) = inbox.recv().await {
        println!("{}: {}", msg.channel, String::from_utf8_lossy(&msg.payload));
        msg.accept();
    }
    Ok(())
}
