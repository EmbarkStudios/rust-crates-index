use crate::{Crate, Error};
use std::{
    io,
    path::{Path, PathBuf},
};

pub struct BareIndex {
    path: PathBuf,
    pub url: String,
}

impl BareIndex {
    /// Creates a bare index from a provided URL, opening the same location on
    /// disk that cargo uses for that registry index.
    pub fn from_url(url: &str) -> Result<Self, Error> {
        let (dir_name, canonical_url) = url_to_local_dir(url)?;
        let mut path = home::cargo_home().unwrap_or_default();

        path.push("registry/index");
        path.push(dir_name);

        Ok(Self {
            path,
            url: canonical_url,
        })
    }

    /// Creates a bare index at the provided path with the specified repository URL.
    pub fn with_path(path: PathBuf, url: &str) -> Self {
        Self {
            path,
            url: url.to_owned(),
        }
    }

    /// Creates an index for the default crates.io registry, using the same
    /// disk location as cargo itself.
    pub fn new_cargo_default() -> Result<Self, Error> {
        Self::from_url(crate::INDEX_GIT_URL)
    }

    /// Opens the local index, which acts as a kind of lock for source control
    /// operations
    pub fn open_or_clone(&self) -> Result<BareIndexRepo<'_>, Error> {
        BareIndexRepo::new(self)
    }
}

pub struct BareIndexRepo<'a> {
    inner: &'a BareIndex,
    head: git2::Oid,
    repo: git2::Repository,
    /// This is safe as we implement Drop manually, dropping this tree
    /// reference before we drop the actual repo that its lifetime is actually
    /// tied to, which is basically the same way that cargo works
    tree: Option<git2::Tree<'static>>,
    head_str: String,
}

impl<'a> BareIndexRepo<'a> {
    fn new(index: &'a BareIndex) -> Result<Self, Error> {
        let exists = git2::Repository::discover(&index.path)
            .map(|repository| {
                repository
                    .find_remote("origin")
                    .ok()
                    // Cargo creates a checkout without an origin set,
                    // so default to true in case of missing origin
                    .map_or(true, |remote| {
                        remote.url().map_or(true, |url| url == index.url)
                    })
            })
            .unwrap_or(false);

        if !exists {
            git2::build::RepoBuilder::new()
                .fetch_options(crate::fetch_opts())
                .bare(true)
                .clone(&index.url, &index.path)?;
        }

        let repo = git2::Repository::open(&index.path)?;
        let head = repo
            // Fallback to HEAD, as a fresh clone won't have a FETCH_HEAD
            .refname_to_id("FETCH_HEAD")
            .or_else(|_| repo.refname_to_id("HEAD"))?;
        let head_str = head.to_string();

        let tree = {
            let commit = repo.find_commit(head)?;
            let tree = commit.tree()?;

            // See comment on self.tree
            unsafe { std::mem::transmute::<git2::Tree<'_>, git2::Tree<'static>>(tree) }
        };

        Ok(Self {
            inner: index,
            head,
            head_str,
            repo,
            tree: Some(tree),
        })
    }

    /// Fetches latest from the remote index repository. Note that using this
    /// method will mean no cache entries will be used, if a new commit is fetched
    /// from the repository, as their commit version will no longer match.
    pub fn retrieve(&mut self) -> Result<(), Error> {
        {
            let mut origin_remote = self
                .repo
                .find_remote("origin")
                .or_else(|_| self.repo.remote_anonymous(&self.inner.url))?;

            origin_remote.fetch(
                &["+refs/heads/*:refs/remotes/origin/*"],
                Some(&mut crate::fetch_opts()),
                None,
            )?;
        }

        let head = self
            .repo
            .refname_to_id("FETCH_HEAD")
            .or_else(|_| self.repo.refname_to_id("HEAD"))?;
        let head_str = head.to_string();

        let commit = self.repo.find_commit(head)?;
        let tree = commit.tree()?;

        // See comment on self.tree
        let tree = unsafe { std::mem::transmute::<git2::Tree<'_>, git2::Tree<'static>>(tree) };

        self.head = head;
        self.head_str = head_str;
        self.tree = Some(tree);

        Ok(())
    }

    /// Reads a crate from the index, it will attempt to use a cached entry if
    /// one is available, otherwise it will fallback to reading the crate
    /// directly from the git blob containing the crate information.
    pub fn crate_(&self, name: &str) -> Option<Crate> {
        let rel_path = match crate::crate_name_to_relative_path(name) {
            Some(rp) => rp,
            None => return None,
        };

        // Attempt to load the .cache/ entry first, this is purely an acceleration
        // mechanism and can fail for a few reasons that are non-fatal
        {
            let mut cache_path = self.inner.path.join(".cache");
            cache_path.push(&rel_path);
            if let Ok(cache_bytes) = std::fs::read(&cache_path) {
                if let Ok(krate) = Crate::from_cache_slice(&cache_bytes, &self.head_str) {
                    return Some(krate);
                }
            }
        }

        // Fallback to reading the blob directly via git if we don't have a
        // valid cache entry
        self.krate_from_blob(&rel_path).ok()
    }

    fn krate_from_blob(&self, path: &str) -> Result<Crate, Error> {
        let entry = self.tree.as_ref().unwrap().get_path(&Path::new(path))?;
        let object = entry.to_object(&self.repo)?;
        let blob = object
            .as_blob()
            .ok_or_else(|| Error::Io(io::Error::new(io::ErrorKind::NotFound, path.to_owned())))?;

        Crate::from_slice(blob.content()).map_err(Error::Io)
    }
}

impl<'a> Drop for BareIndexRepo<'a> {
    fn drop(&mut self) {
        // Just be sure to drop this before our other fields
        self.tree.take();
    }
}

/// Converts a full url, eg https://github.com/rust-lang/crates.io-index, into
/// the root directory name where cargo itself will fetch it on disk
fn url_to_local_dir(url: &str) -> Result<(String, String), Error> {
    fn to_hex(num: u64) -> String {
        const CHARS: &[u8] = b"0123456789abcdef";

        let bytes = &[
            num as u8,
            (num >> 8) as u8,
            (num >> 16) as u8,
            (num >> 24) as u8,
            (num >> 32) as u8,
            (num >> 40) as u8,
            (num >> 48) as u8,
            (num >> 56) as u8,
        ];

        let mut output = vec![0u8; 16];

        let mut ind = 0;

        for &byte in bytes {
            output[ind] = CHARS[(byte >> 4) as usize];
            output[ind + 1] = CHARS[(byte & 0xf) as usize];

            ind += 2;
        }

        String::from_utf8(output).expect("valid utf-8 hex string")
    }

    #[allow(deprecated)]
    fn hash_u64(url: &str) -> u64 {
        use std::hash::{Hash, Hasher, SipHasher};

        let mut hasher = SipHasher::new_with_keys(0, 0);
        // Registry
        2usize.hash(&mut hasher);
        // Url
        url.hash(&mut hasher);
        hasher.finish()
    }

    // Ensure we have a registry or bare url
    let (url, scheme_ind) = {
        let scheme_ind = url
            .find("://")
            .ok_or_else(|| Error::Url(format!("'{}' is not a valid url", url)))?;

        let scheme_str = &url[..scheme_ind];
        if let Some(ind) = scheme_str.find('+') {
            if &scheme_str[..ind] != "registry" {
                return Err(Error::Url(format!("'{}' is not a valid registry url", url)));
            }

            (&url[ind + 1..], scheme_ind - ind - 1)
        } else {
            (url, scheme_ind)
        }
    };

    // Could use the Url crate for this, but it's simple enough and we don't
    // need to deal with every possible url (I hope...)
    let host = match url[scheme_ind + 3..].find('/') {
        Some(end) => &url[scheme_ind + 3..scheme_ind + 3 + end],
        None => &url[scheme_ind + 3..],
    };

    // cargo special cases github.com for reasons, so do the same
    let mut canonical = if host == "github.com" {
        url.to_lowercase()
    } else {
        url.to_owned()
    };

    // Chop off any query params/fragments
    if let Some(hash) = canonical.rfind('#') {
        canonical.truncate(hash);
    }

    if let Some(query) = canonical.rfind('?') {
        canonical.truncate(query);
    }

    let ident = to_hex(hash_u64(&canonical));

    if canonical.ends_with('/') {
        canonical.pop();
    }

    if canonical.ends_with(".git") {
        canonical.truncate(canonical.len() - 4);
    }

    Ok((format!("{}-{}", host, ident), canonical))
}

#[cfg(test)]
mod test {
    #[test]
    fn matches_cargo() {
        assert_eq!(
            super::url_to_local_dir(crate::INDEX_GIT_URL).unwrap(),
            (
                "github.com-1ecc6299db9ec823".to_owned(),
                crate::INDEX_GIT_URL.to_owned()
            )
        );

        // I've confirmed this also works with a custom registry, unfortunately
        // that one includes a secret key as part of the url which would allow
        // anyone to publish to the registry, so uhh...here's a fake one instead
        assert_eq!(
            super::url_to_local_dir(
                "https://dl.cloudsmith.io/aBcW1234aBcW1234/embark/rust/cargo/index.git"
            )
            .unwrap(),
            (
                "dl.cloudsmith.io-ff79e51ddd2b38fd".to_owned(),
                "https://dl.cloudsmith.io/aBcW1234aBcW1234/embark/rust/cargo/index".to_owned()
            )
        );

        // Ensure we actually strip off the irrelevant parts of a url, note that
        // the .git suffix is not part of the canonical url, but *is* used when hashing
        assert_eq!(
            super::url_to_local_dir(&format!(
                "registry+{}.git?one=1&two=2#fragment",
                crate::INDEX_GIT_URL
            ))
            .unwrap(),
            (
                "github.com-c786010fb7ef2e6e".to_owned(),
                crate::INDEX_GIT_URL.to_owned()
            )
        );
    }

    #[test]
    fn clones_bare_index() {
        use super::BareIndex;

        let tmp_dir = tempdir::TempDir::new("clones_bare_index").unwrap();

        let index = BareIndex::with_path(tmp_dir.path().to_owned(), crate::INDEX_GIT_URL);

        let mut repo = index
            .open_or_clone()
            .expect("Failed to clone crates.io index");

        fn test_sval(repo: &super::BareIndexRepo<'_>) {
            let krate = repo
                .crate_("sval")
                .expect("Could not find the crate sval in the index");

            let version = krate
                .versions()
                .iter()
                .find(|v| v.version() == "0.0.1")
                .expect("Version 0.0.1 of sval does not exist?");
            let dep_with_package_name = version
                .dependencies()
                .iter()
                .find(|d| d.name() == "serde_lib")
                .expect("sval does not have expected dependency?");
            assert_ne!(
                dep_with_package_name.name(),
                dep_with_package_name.package().unwrap()
            );
            assert_eq!(
                dep_with_package_name.crate_name(),
                dep_with_package_name.package().unwrap()
            );
        }

        test_sval(&repo);

        repo.retrieve().expect("Failed to fetch crates.io index");

        test_sval(&repo);
    }

    #[test]
    fn opens_bare_index() {
        use super::BareIndex;

        let tmp_dir = tempdir::TempDir::new("opens_bare_index").unwrap();

        let index = BareIndex::with_path(tmp_dir.path().to_owned(), crate::INDEX_GIT_URL);

        {
            let _ = index
                .open_or_clone()
                .expect("Failed to clone crates.io index");
        }

        let mut repo = index
            .open_or_clone()
            .expect("Failed to open crates.io index");

        fn test_sval(repo: &super::BareIndexRepo<'_>) {
            let krate = repo
                .crate_("sval")
                .expect("Could not find the crate sval in the index");

            let version = krate
                .versions()
                .iter()
                .find(|v| v.version() == "0.0.1")
                .expect("Version 0.0.1 of sval does not exist?");
            let dep_with_package_name = version
                .dependencies()
                .iter()
                .find(|d| d.name() == "serde_lib")
                .expect("sval does not have expected dependency?");
            assert_ne!(
                dep_with_package_name.name(),
                dep_with_package_name.package().unwrap()
            );
            assert_eq!(
                dep_with_package_name.crate_name(),
                dep_with_package_name.package().unwrap()
            );
        }

        test_sval(&repo);

        repo.retrieve().expect("Failed to fetch crates.io index");

        test_sval(&repo);
    }
}
