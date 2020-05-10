use std::time::Instant;

mod new_reno;
pub use new_reno::{NewReno, NewRenoConfig};

/// Logic and state controlling the maximum amount of data in flight
pub trait Controller: Send {
    /// Packet deliveries were confirmed
    fn on_ack(&mut self, sent: Instant, bytes: u64, congestion_blocked: bool);

    /// Packets were deemed lost or marked congested
    fn on_congestion_event(&mut self, now: Instant, sent: Instant, persistent: bool);

    /// Number of ack-eliciting bytes that may be in flight
    fn window(&self) -> u64;

    /// Duplicate the controller's state
    fn clone_box(&self) -> Box<dyn Controller>;
}
