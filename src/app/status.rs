//! The one transient status message.

#[derive(Debug, Default)]
pub struct Status {
    message: Option<String>,
}

impl Status {
    pub fn set(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    pub fn clear(&mut self) {
        self.message = None;
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}
