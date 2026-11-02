//! Synchronous adapter over the upstream asynchronous Secret Service implementation.
//! Connections live on a dedicated, persistent runtime, including when the caller
//! is already inside a current-thread Tokio runtime. Secret formats, DH sessions,
//! attributes, prompts and encryption remain implemented by the async API.
use crate::{EncryptionType, Error, SearchItemsResult};
use std::{collections::HashMap, future::Future, sync::OnceLock};
use zbus::OwnedObjectPath;
mod collection;
mod item;
pub use collection::Collection;
pub use item::Item;

fn runtime_error(message: impl Into<String>) -> Error {
    Error::Zbus(zbus::Error::Failure(message.into()))
}
pub(crate) fn run<T: Send, F: Future<Output = Result<T, Error>> + Send>(
    future: F,
) -> Result<T, Error> {
    static RUNTIME: OnceLock<std::io::Result<tokio::runtime::Runtime>> = OnceLock::new();
    let runtime = RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("cshell-secret-service")
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|error| runtime_error(format!("Secret Service runtime unavailable: {error}")))?;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("cshell-keyring-call".into())
            .spawn_scoped(scope, move || runtime.block_on(future))
            .map_err(|error| runtime_error(format!("Secret Service worker unavailable: {error}")))?
            .join()
            .map_err(|_| runtime_error("Secret Service worker panicked"))?
    })
}

pub struct SecretService<'a> {
    inner: crate::SecretService<'a>,
}
impl<'a> std::ops::Deref for SecretService<'a> {
    type Target = crate::SecretService<'a>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<'a> SecretService<'a> {
    pub fn connect(encryption: EncryptionType) -> Result<Self, Error> {
        run(crate::SecretService::connect(encryption)).map(|inner| Self { inner })
    }
    pub fn connect_with_existing(
        encryption: EncryptionType,
        connection: zbus::Connection,
    ) -> Result<Self, Error> {
        run(crate::SecretService::connect_with_existing(
            encryption, connection,
        ))
        .map(|inner| Self { inner })
    }
    pub fn get_all_collections(&self) -> Result<Vec<Collection<'_>>, Error> {
        run(self.inner.get_all_collections())
            .map(|items| items.into_iter().map(Collection::from).collect())
    }
    pub fn get_collection_by_alias(&self, alias: &str) -> Result<Collection<'_>, Error> {
        run(self.inner.get_collection_by_alias(alias)).map(Collection::from)
    }
    pub fn get_default_collection(&self) -> Result<Collection<'_>, Error> {
        run(self.inner.get_default_collection()).map(Collection::from)
    }
    pub fn get_any_collection(&self) -> Result<Collection<'_>, Error> {
        run(self.inner.get_any_collection()).map(Collection::from)
    }
    pub fn create_collection(&self, label: &str, alias: &str) -> Result<Collection<'_>, Error> {
        run(self.inner.create_collection(label, alias)).map(Collection::from)
    }
    pub fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> Result<SearchItemsResult<Item<'_>>, Error> {
        let result = run(self.inner.search_items(attributes))?;
        Ok(SearchItemsResult {
            unlocked: result.unlocked.into_iter().map(Item::from).collect(),
            locked: result.locked.into_iter().map(Item::from).collect(),
        })
    }
    pub fn unlock_all(&self, items: &[&Item<'_>]) -> Result<(), Error> {
        let items = items.iter().map(|item| &item.inner).collect::<Vec<_>>();
        run(self.inner.unlock_all(&items))
    }
    pub fn get_item_by_path(&'a self, path: OwnedObjectPath) -> Result<Item<'a>, Error> {
        run(self.inner.get_item_by_path(path)).map(Item::from)
    }
    pub fn get_collection_by_path(
        &'a self,
        path: OwnedObjectPath,
    ) -> Result<Collection<'a>, Error> {
        run(self.inner.get_collection_by_path(path)).map(Collection::from)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::convert::TryFrom;
    use zbus::wire::ObjectPath;

    #[test]
    fn should_create_secret_service() {
        SecretService::connect(EncryptionType::Plain).unwrap();
    }

    #[test]
    fn should_get_all_collections() {
        // Assumes that there will always be a default
        // collection
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let collections = ss.get_all_collections().unwrap();
        assert!(!collections.is_empty(), "no collections found");
    }

    #[test]
    fn should_get_collection_by_alias() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        ss.get_collection_by_alias("session").unwrap();
    }

    #[test]
    fn should_return_error_if_collection_doesnt_exist() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();

        match ss.get_collection_by_alias("definitely_definitely_does_not_exist") {
            Err(Error::NoResult) => {}
            _ => panic!(),
        };
    }

    #[test]
    fn should_get_default_collection() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        ss.get_default_collection().unwrap();
    }

    #[test]
    fn should_get_any_collection() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let _ = ss.get_any_collection().unwrap();
    }

    #[test_with::no_env(GITHUB_ACTIONS)]
    #[test]
    fn should_create_and_delete_collection() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let test_collection = ss.create_collection("Test", "").unwrap();
        assert_eq!(
            ObjectPath::from(test_collection.collection_path.clone()),
            ObjectPath::try_from("/org/freedesktop/secrets/collection/Test").unwrap()
        );
        test_collection.delete().unwrap();
    }

    #[test]
    fn should_search_items() {
        let ss = SecretService::connect(EncryptionType::Dh).unwrap();
        let collection = ss.get_default_collection().unwrap();

        // Create an item
        let item = collection
            .create_item(
                "test",
                HashMap::from([("test_attribute_in_ss", "test_value")]),
                b"test_secret",
                false,
                "text/plain",
            )
            .unwrap();

        // handle empty vec search
        ss.search_items(HashMap::new()).unwrap();

        // handle no result
        let bad_search = ss.search_items(HashMap::from([("test", "test")])).unwrap();
        assert_eq!(bad_search.unlocked.len(), 0);
        assert_eq!(bad_search.locked.len(), 0);

        // handle correct search for item and compare
        let search_item = ss
            .search_items(HashMap::from([("test_attribute_in_ss", "test_value")]))
            .unwrap();

        assert_eq!(item.item_path, search_item.unlocked[0].item_path);
        assert_eq!(search_item.locked.len(), 0);
        item.delete().unwrap();
    }
}
