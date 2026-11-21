pub mod noop;
#[cfg(target_os = "linux")]
pub mod uinput_linux;
#[cfg(target_os = "linux")]
pub mod wayland_virtual;
#[cfg(target_os = "windows")]
pub mod win32_input;

use usher_proto::input::InputEvent;

pub trait InputSink: Send {
    fn handle(&mut self, event: InputEvent);
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub fn create_input_sink_for_session(
    enabled: bool,
    wayland_socket: Option<&std::path::Path>,
) -> Box<dyn InputSink> {
    if !enabled {
        return Box::new(noop::NoopInputSink::default());
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(sock) = wayland_socket {
            match wayland_virtual::WaylandVirtualInputSink::connect(sock) {
                Ok(sink) => {
                    tracing::info!(
                        socket = %sock.display(),
                        "registered Wayland virtual pointer and keyboard (zero /dev/uinput needed)"
                    );
                    return Box::new(sink);
                }
                Err(err) => {
                    tracing::warn!(
                        "Wayland virtual input unavailable ({err:#}); falling back to /dev/uinput"
                    );
                }
            }
        }
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

    #[cfg(target_os = "windows")]
    {
        if crate::windows_session::current_session_id() == 0 {
            tracing::warn!(
                "running in Windows Session 0; SendInput disabled (using no-op input)"
            );
        } else {
            tracing::info!("registered Win32 SendInput virtual keyboard and mouse");
            return Box::new(win32_input::Win32InputSink::new());
        }
    }

    Box::new(noop::NoopInputSink::default())
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn create_input_sink(enabled: bool) -> Box<dyn InputSink> {
    create_input_sink_for_session(enabled, None)
}
