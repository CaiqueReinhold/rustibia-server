use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Instant;

use super::metrics;

const SINK_BUFFER: usize = 64;

#[derive(Debug)]
pub struct CommandRecord {
    pub command: &'static str,
    pub start: Instant,
    pub end: Instant,
}

#[derive(Clone, Debug)]
pub struct CommandSink {
    tx: SyncSender<Vec<CommandRecord>>,
}

impl CommandSink {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel(SINK_BUFFER);
        std::thread::Builder::new()
            .name("command-sink".into())
            .spawn(move || drain(rx))
            .expect("spawning the command sink thread");
        Self { tx }
    }

    #[cfg(test)]
    pub fn for_test() -> (Self, Receiver<Vec<CommandRecord>>) {
        let (tx, rx) = mpsc::sync_channel(SINK_BUFFER);
        (Self { tx }, rx)
    }

    /// Never waits. A batch that does not fit is dropped and counted.
    pub fn submit(&self, records: Vec<CommandRecord>) -> bool {
        let accepted = self.tx.try_send(records).is_ok();
        if !accepted {
            metrics().record_dropped_batch();
        }
        accepted
    }
}

fn drain(rx: Receiver<Vec<CommandRecord>>) {
    while let Ok(records) = rx.recv() {
        metrics().record_commands(&records);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_sink_refuses_the_batch_instead_of_waiting() {
        let (sink, _rx) = CommandSink::for_test();
        for _ in 0..SINK_BUFFER {
            assert!(sink.submit(Vec::new()));
        }

        assert!(!sink.submit(Vec::new()));
    }
}
