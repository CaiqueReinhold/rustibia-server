use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::actors::persistence::PersistenceActorHandle;
use crate::entities::player::PlayerId;

#[derive(Clone)]
pub struct OnlineRegistry {
    inner: Arc<Mutex<HashSet<PlayerId>>>,
    persistence: PersistenceActorHandle,
}

#[derive(Debug)]
pub struct RegistryGuard {
    inner: Arc<Mutex<HashSet<PlayerId>>>,
    persistence: PersistenceActorHandle,
    character_id: PlayerId,
}

impl OnlineRegistry {
    pub fn new(persistence: PersistenceActorHandle) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashSet::new())),
            persistence,
        }
    }

    /// Returns Some(guard) if character_id was not already registered.
    /// Returns None if the character is already online.
    /// The returned guard removes the entry — in memory and in the database — on drop.
    pub fn try_register(&self, character_id: PlayerId) -> Option<RegistryGuard> {
        let mut set = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if set.insert(character_id) {
            drop(set);
            self.persistence.mark_online(character_id);
            Some(RegistryGuard {
                inner: Arc::clone(&self.inner),
                persistence: self.persistence.clone(),
                character_id,
            })
        } else {
            None
        }
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.inner.lock() {
            set.remove(&self.character_id);
        }
        self.persistence.mark_offline(self.character_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::persistence::PersistenceCommand;
    use tokio::sync::mpsc;

    fn a_registry() -> (OnlineRegistry, mpsc::Receiver<PersistenceCommand>) {
        let (handle, rx) = PersistenceActorHandle::for_test(16);
        (OnlineRegistry::new(handle), rx)
    }

    fn assert_send<T: Send>() {}

    #[test]
    fn test_registry_guard_is_send() {
        assert_send::<RegistryGuard>();
    }

    #[test]
    fn test_first_register_succeeds() {
        let (registry, mut rx) = a_registry();

        let guard = registry.try_register(PlayerId(1));

        assert!(guard.is_some());
        assert!(
            matches!(
                rx.try_recv(),
                Ok(PersistenceCommand::MarkOnline(PlayerId(1)))
            ),
            "registering must publish MarkOnline so the website's player count updates"
        );
    }

    #[test]
    fn test_duplicate_register_fails_while_guard_alive() {
        let (registry, mut rx) = a_registry();

        let _first = registry
            .try_register(PlayerId(1))
            .expect("first register succeeds");
        let second = registry.try_register(PlayerId(1));

        assert!(
            second.is_none(),
            "the same character must not be online twice"
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(PersistenceCommand::MarkOnline(PlayerId(1)))
        ));
        assert!(
            rx.try_recv().is_err(),
            "a rejected duplicate must not publish a second MarkOnline"
        );
    }

    #[test]
    fn test_drop_releases_slot() {
        let (registry, mut rx) = a_registry();

        let guard = registry
            .try_register(PlayerId(1))
            .expect("first register succeeds");
        drop(guard);

        let second = registry.try_register(PlayerId(1));
        assert!(
            second.is_some(),
            "dropping the guard must free the slot for a reconnect"
        );

        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            matches!(
                sent.as_slice(),
                [
                    PersistenceCommand::MarkOnline(PlayerId(1)),
                    PersistenceCommand::MarkOffline(PlayerId(1)),
                    PersistenceCommand::MarkOnline(PlayerId(1))
                ]
            ),
            "the guard's Drop must publish MarkOffline between the two logins, or the \
             website shows a player online forever after they disconnect; got {sent:?}"
        );
    }

    #[test]
    fn test_different_characters_can_register_simultaneously() {
        let (registry, _rx) = a_registry();

        let first = registry.try_register(PlayerId(1));
        let second = registry.try_register(PlayerId(2));

        assert!(first.is_some());
        assert!(
            second.is_some(),
            "distinct characters must not block each other"
        );
    }
}
