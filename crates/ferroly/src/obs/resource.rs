//! The resource: who is emitting the telemetry.

use ferroly::codec::Value;

/// Attributes describing the entity that produces spans — the service, its
/// version, the instance, the environment. Exporters attach them to every
/// batch.
///
/// ```
/// use ferroly::obs::Resource;
///
/// let resource = Resource::new("checkout")
///     .service_version("1.4.2")
///     .service_instance_id("checkout-7d9f-abcde")
///     .deployment_environment("production")
///     .attribute("cloud.region", "eu-west-1");
/// assert_eq!(resource.service_name(), "checkout");
/// assert_eq!(resource.attributes().len(), 5);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    attributes: Vec<(String, Value)>,
}

impl Resource {
    /// A resource with `service.name` set.
    pub fn new(service_name: impl Into<String>) -> Resource {
        Resource {
            attributes: vec![("service.name".to_string(), Value::Str(service_name.into()))],
        }
    }

    /// Sets `service.version`.
    pub fn service_version(self, version: impl Into<String>) -> Resource {
        self.attribute("service.version", version.into())
    }

    /// Sets `service.instance.id` (unique per running instance).
    pub fn service_instance_id(self, id: impl Into<String>) -> Resource {
        self.attribute("service.instance.id", id.into())
    }

    /// Sets `deployment.environment` (`"production"`, `"staging"`, …).
    pub fn deployment_environment(self, environment: impl Into<String>) -> Resource {
        self.attribute("deployment.environment", environment.into())
    }

    /// Sets an arbitrary attribute, replacing an existing one with the same key.
    pub fn attribute(mut self, key: impl Into<String>, value: impl Into<Value>) -> Resource {
        let (key, value) = (key.into(), value.into());
        match self.attributes.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.attributes.push((key, value)),
        }
        self
    }

    /// The `service.name` attribute.
    pub fn service_name(&self) -> &str {
        self.attributes
            .iter()
            .find(|(k, _)| k == "service.name")
            .and_then(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// Every attribute, in insertion order.
    pub fn attributes(&self) -> &[(String, Value)] {
        &self.attributes
    }
}
