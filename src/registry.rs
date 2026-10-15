//! Explicit, injected factories. Registration never performs I/O or replaces an entry.

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    EmptyId,
    Duplicate(String),
    Unknown { id: String, known: Vec<String> },
    Factory(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => write!(f, "提供方 id 不能为空"),
            Self::Duplicate(id) => write!(f, "提供方已注册：{id}"),
            Self::Unknown { id, known } => {
                write!(f, "未知提供方: {id}。已注册: {}", known.join(", "))
            }
            Self::Factory(message) => message.fmt(f),
        }
    }
}

impl std::error::Error for RegistryError {}

type Factory<C, T> = Arc<dyn Fn(C) -> Result<T, RegistryError> + Send + Sync>;

pub struct FactoryRegistry<C, T> {
    entries: Vec<(String, Factory<C, T>)>,
}

impl<C, T> Default for FactoryRegistry<C, T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<C, T> FactoryRegistry<C, T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        id: &str,
        factory: impl Fn(C) -> Result<T, RegistryError> + Send + Sync + 'static,
    ) -> Result<(), RegistryError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(RegistryError::EmptyId);
        }
        if self.contains(id) {
            return Err(RegistryError::Duplicate(id.into()));
        }
        self.entries.push((id.into(), Arc::new(factory)));
        Ok(())
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|(key, _)| key == id)
    }

    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|(id, _)| id.clone()).collect()
    }

    pub fn create(&self, id: &str, context: C) -> Result<T, RegistryError> {
        let factory = self
            .entries
            .iter()
            .find(|(key, _)| key == id)
            .ok_or_else(|| RegistryError::Unknown {
                id: id.into(),
                known: self.ids(),
            })?;
        (factory.1)(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn factories_are_lazy_repeatable_and_registration_is_unambiguous() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mut registry = FactoryRegistry::new();
        registry
            .register("a", move |value: usize| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(value))
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            registry.register(" a ", |_| Ok(Box::new(0))),
            Err(RegistryError::Duplicate(_))
        ));
        assert!(matches!(
            registry.register(" ", |_| Ok(Box::new(0))),
            Err(RegistryError::EmptyId)
        ));
        assert_eq!(*registry.create("a", 1).unwrap(), 1);
        assert_eq!(*registry.create("a", 2).unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            matches!(registry.create("missing", 0), Err(RegistryError::Unknown { known, .. }) if known == ["a"])
        );
    }
}
