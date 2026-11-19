use super::InputSink;
use usher_proto::input::InputEvent;

#[derive(Default)]
pub struct NoopInputSink {
    pub count: u64,
}

impl InputSink for NoopInputSink {
    fn handle(&mut self, event: InputEvent) {
        self.count += 1;
        if self.count <= 3 || self.count.is_multiple_of(500) {
            tracing::debug!(count = self.count, ?event, "noop input event");
        }
    }
}
