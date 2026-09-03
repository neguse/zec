//! Every user-invocable operation, and its GPUI action for the keymap.
//!
//! Adding a command means adding one line to [`commands!`]; the action, its
//! name, the palette label, and the keymap interceptor follow from it.

use std::{cell::RefCell, rc::Rc};

use gpui::App;

macro_rules! commands {
    ($( $variant:ident => $label:literal, )*) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum Command {
            $( $variant, )*
        }

        /// GPUI actions in the `zec` namespace, one per command.
        pub mod actions {
            gpui::actions!(zec, [ $( $variant, )* ]);
        }

        impl Command {
            pub const ALL: &'static [Command] = &[ $( Command::$variant, )* ];

            /// The action name as written in a keymap file.
            pub fn action_name(self) -> &'static str {
                match self {
                    $( Command::$variant => concat!("zec::", stringify!($variant)), )*
                }
            }

            pub fn label(self) -> &'static str {
                match self {
                    $( Command::$variant => $label, )*
                }
            }

            pub fn from_action_name(name: &str) -> Option<Self> {
                match name {
                    $( concat!("zec::", stringify!($variant)) => Some(Command::$variant), )*
                    _ => None,
                }
            }

            /// Turns every dispatched zec action into a pending command
            /// instead of letting a hidden Editor or a future GUI run it.
            pub fn intercept(pending: Rc<RefCell<Vec<Command>>>, cx: &mut App) {
                $(
                    {
                        let pending = pending.clone();
                        cx.on_action::<actions::$variant>(move |_, cx| {
                            pending.borrow_mut().push(Command::$variant);
                            cx.stop_propagation();
                        });
                    }
                )*
            }
        }
    };
}

commands! {
    CommandPalette => "Command Palette",
    NewFile => "New File",
    OpenFile => "Open File",
    QuickOpen => "Quick Open",
    ProjectSearch => "Search Project",
    Save => "Save",
    SaveAs => "Save As",
    Reload => "Reload From Disk",
    CloseItem => "Close Tab",
    NextItem => "Next Tab",
    PreviousItem => "Previous Tab",
    Copy => "Copy",
    Cut => "Cut",
    OpenSettings => "Open Settings File",
    OpenKeymap => "Open Keymap File",
    ShowTerminalCapabilities => "Show Terminal Capabilities",
    Quit => "Quit",
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn action_names_are_unique_and_round_trip() {
        let names = Command::ALL
            .iter()
            .map(|command| command.action_name())
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), Command::ALL.len());
        for command in Command::ALL {
            assert_eq!(
                Command::from_action_name(command.action_name()),
                Some(*command)
            );
            assert!(!command.label().is_empty());
        }
    }
}
