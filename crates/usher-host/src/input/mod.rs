pub mod noop;
#[cfg(target_os = "linux")]
pub mod uinput_linux;

use usher_proto::input::InputEvent;

pub trait InputSink: Send {
    fn handle(&mut self, event: InputEvent);
}

pub fn create_input_sink(enabled: bool) -> Box<dyn InputSink> {
    if !enabled {
        return Box::new(noop::NoopInputSink::default());
    }

    #[cfg(target_os = "linux")]
    {
        match uinput_linux::UinputSink::new() {
            Ok(sink) => {
                tracing::info!("registered Linux /dev/uinput virtual keyboard, mice, and gamepad");
                return Box::new(sink);
            }
            Err(err) => {
                tracing::warn!(
                    "failed to initialize /dev/uinput ({err:#}); continuing with no-op input. \
                     Ensure udev rule is installed: KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\""
                );
            }
        }
    }

    Box::new(noop::NoopInputSink::default())
}
