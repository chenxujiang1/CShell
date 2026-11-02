//! Synchronous wrapper around the maintained async Collection implementation.
use super::run;
use crate::Error;
use std::collections::HashMap;
pub struct Collection<'a> {
    pub(super) inner: crate::Collection<'a>,
}
impl<'a> From<crate::Collection<'a>> for Collection<'a> {
    fn from(inner: crate::Collection<'a>) -> Self {
        Self { inner }
    }
}
impl<'a> std::ops::Deref for Collection<'a> {
    type Target = crate::Collection<'a>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<'a> Collection<'a> {
    pub fn is_locked(&self) -> Result<bool, Error> {
        run(self.inner.is_locked())
    }
    pub fn ensure_unlocked(&self) -> Result<(), Error> {
        run(self.inner.ensure_unlocked())
    }
    pub fn unlock(&self) -> Result<(), Error> {
        run(self.inner.unlock())
    }
    pub fn lock(&self) -> Result<(), Error> {
        run(self.inner.lock())
    }
    pub fn delete(&self) -> Result<(), Error> {
        run(self.inner.delete())
    }
    pub fn get_label(&self) -> Result<String, Error> {
        run(self.inner.get_label())
    }
    pub fn set_label(&self, label: &str) -> Result<(), Error> {
        run(self.inner.set_label(label))
    }
    pub fn get_all_items(&self) -> Result<Vec<super::Item<'_>>, Error> {
        run(self.inner.get_all_items())
            .map(|items| items.into_iter().map(super::Item::from).collect())
    }
    pub fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> Result<Vec<super::Item<'_>>, Error> {
        run(self.inner.search_items(attributes))
            .map(|items| items.into_iter().map(super::Item::from).collect())
    }
    pub fn create_item(
        &self,
        label: &str,
        attributes: HashMap<&str, &str>,
        secret: &[u8],
        replace: bool,
        content_type: &str,
    ) -> Result<super::Item<'_>, Error> {
        run(self
            .inner
            .create_item(label, attributes, secret, replace, content_type))
        .map(super::Item::from)
    }
}

#[cfg(test)]
mod test {
    use crate::blocking::*;

    #[test]
    fn should_create_collection_struct() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let _ = ss.get_default_collection().unwrap();
        // tested under SecretService struct
    }

    #[test]
    fn should_check_if_collection_locked() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let collection = ss.get_default_collection().unwrap();
        let _ = collection.is_locked().unwrap();
    }

    #[test]
    #[ignore] // should unignore this test this manually, otherwise will constantly prompt during tests.
    fn should_lock_and_unlock() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let collection = ss.get_default_collection().unwrap();
        let locked = collection.is_locked().unwrap();
        if locked {
            collection.unlock().unwrap();
            collection.ensure_unlocked().unwrap();
            assert!(!collection.is_locked().unwrap());
            collection.lock().unwrap();
            assert!(collection.is_locked().unwrap());
        } else {
            collection.lock().unwrap();
            assert!(collection.is_locked().unwrap());
            collection.unlock().unwrap();
            collection.ensure_unlocked().unwrap();
            assert!(!collection.is_locked().unwrap());
        }
    }

    #[test]
    #[ignore]
    fn should_delete_collection() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let collections = ss.get_all_collections().unwrap();
        let count_before = collections.len();
        for collection in collections {
            let collection_path = &*collection.collection_path;
            if collection_path.contains("Test") {
                collection.unlock().unwrap();
                collection.delete().unwrap();
            }
        }
        //double check after
        let collections = ss.get_all_collections().unwrap();
        assert!(
            collections.len() < count_before,
            "collections before delete {count_before}",
        )
    }

    #[test]
    fn should_get_all_items() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let collection = ss.get_default_collection().unwrap();
        collection.get_all_items().unwrap();
    }

    #[test]
    fn should_search_items() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let collection = ss.get_default_collection().unwrap();

        // Create an item
        let item = collection
            .create_item(
                "test",
                HashMap::from([("test_attributes_in_collection", "test")]),
                b"test_secret",
                false,
                "text/plain",
            )
            .unwrap();

        // handle empty vec search
        collection.search_items(HashMap::new()).unwrap();

        // handle no result
        let bad_search = collection
            .search_items(HashMap::from([("test_bad", "test")]))
            .unwrap();
        assert_eq!(bad_search.len(), 0);

        // handle correct search for item and compare
        let search_item = collection
            .search_items(HashMap::from([("test_attributes_in_collection", "test")]))
            .unwrap();

        assert_eq!(item.item_path, search_item[0].item_path);
        item.delete().unwrap();
    }

    #[test]
    #[ignore]
    fn should_get_and_set_collection_label() {
        let ss = SecretService::connect(EncryptionType::Plain).unwrap();
        let collection = ss.get_default_collection().unwrap();
        let label = collection.get_label().unwrap();
        assert_eq!(label, "Login");

        // Set label to test and check
        collection.unlock().unwrap();
        collection.set_label("Test").unwrap();
        let label = collection.get_label().unwrap();
        assert_eq!(label, "Test");

        // Reset label to original and test
        collection.unlock().unwrap();
        collection.set_label("Login").unwrap();
        let label = collection.get_label().unwrap();
        assert_eq!(label, "Login");

        collection.lock().unwrap();
    }

    #[test]
    fn should_get_collection_by_path() {
        let path: OwnedObjectPath;
        let label: String;
        // get the default collection on one blocking call, remember its path and label
        {
            let ss = SecretService::connect(EncryptionType::Plain).unwrap();
            let collection = ss.get_default_collection().unwrap();
            label = collection.get_label().unwrap();
            path = collection.collection_path.clone();
        }
        // get collection by path on another blocking call, check that the label is the same
        {
            let ss = SecretService::connect(EncryptionType::Plain).unwrap();
            let collection_prime = ss.get_collection_by_path(path).unwrap();
            let label_prime = collection_prime.get_label().unwrap();
            assert_eq!(label, label_prime);
        }
    }
}
