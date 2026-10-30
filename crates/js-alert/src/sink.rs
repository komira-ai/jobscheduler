//! The sink trait and the recording sink.

use std::sync::{Mutex, PoisonError};

/// One alert: a key that names the condition and holds while the condition does, and the
/// words an operator reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// The key of the condition, such as `orphan:gpu:js-ab12:i-1`.
    pub key: String,
    /// What is wrong and what to do, in words.
    pub text: String,
}

/// An alert could not be delivered. The words hold no secret (not the URL, not a token).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SinkError(String);

impl SinkError {
    /// An error that reads as `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Where alerts are delivered. A call blocks until the sink has the alert or has failed.
pub trait AlertSink: Send + Sync {
    /// Delivers `alert`.
    ///
    /// # Errors
    ///
    /// [`SinkError`] when the sink did not take it; the caller sends it again later.
    fn send(&self, alert: &Alert) -> Result<(), SinkError>;
}

/// A sink that keeps what it is given, and refuses on demand.
#[derive(Debug, Default)]
pub struct FakeSink {
    sent: Mutex<Vec<Alert>>,
    refuse: Mutex<usize>,
}

impl FakeSink {
    /// A sink that has received nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next `count` sends fail.
    pub fn refuse_next(&self, count: usize) {
        *self.refuse.lock().unwrap_or_else(PoisonError::into_inner) = count;
    }

    /// The alerts delivered so far, in order.
    pub fn sent(&self) -> Vec<Alert> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The keys of the alerts delivered so far, in order.
    pub fn keys(&self) -> Vec<String> {
        self.sent().into_iter().map(|alert| alert.key).collect()
    }
}

impl AlertSink for FakeSink {
    fn send(&self, alert: &Alert) -> Result<(), SinkError> {
        let mut refuse = self.refuse.lock().unwrap_or_else(PoisonError::into_inner);
        if *refuse > 0 {
            *refuse -= 1;
            return Err(SinkError::new("the fake sink was told to refuse"));
        }
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(alert.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the fake keeps what it is given in order, refuses exactly as many
    /// sends as it was told to and then takes them again, and that an error reads as its
    /// words. It catches a fake that drops alerts or refuses forever, which would make
    /// every test built on it vacuous.
    #[test]
    fn the_fake_keeps_alerts_in_order_and_refuses_on_demand() {
        let sink = FakeSink::new();
        let alert = |key: &str| Alert {
            key: key.to_owned(),
            text: format!("text of {key}"),
        };
        sink.send(&alert("a")).unwrap();
        sink.refuse_next(1);
        let refused = sink.send(&alert("b")).unwrap_err();
        assert_eq!(refused.to_string(), "the fake sink was told to refuse");
        sink.send(&alert("b")).unwrap();
        assert_eq!(sink.keys(), ["a", "b"]);
        assert_eq!(sink.sent()[1].text, "text of b");
    }
}
