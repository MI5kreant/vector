use async_graphql::{Enum, SimpleObject};

#[derive(Enum, Copy, Clone, PartialEq, Eq)]
/// Type of log event error
pub enum LogEventNotificationType {
    /// There isn't currently a component that matches this name
    ComponentNotReady,
    /// Component has been removed from topology
    ComponentWentAway,
    /// Component came back (typically via a configuration reload)
    ComponentCameBack,
}

#[derive(SimpleObject)]
/// A notification regarding logs events observation
pub struct LogEventNotification {
    /// Name of the component associated with the notification
    component_name: String,

    /// Log event notification type
    notification: LogEventNotificationType,
}

impl LogEventNotification {
    pub fn new(component_name: &str, notification: LogEventNotificationType) -> Self {
        Self {
            component_name: component_name.to_string(),
            notification,
        }
    }
}
