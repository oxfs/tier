//! WebDAV object access for an existing collection, including OpenList.
use crate::core::objects;
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    time::Duration,
};
use url::Url;

fn collection_url(collection: &str) -> Result<Url> {
    let url = Url::parse(collection).context("invalid WebDAV collection URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "WebDAV URL must use HTTP(S)"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "use a plain collection URL without embedded credentials"
    );
    Ok(url)
}

/// Append one opaque object name, escaping URL delimiters in the original name.
pub(crate) fn object_url(collection: &str, key: &str) -> Result<String> {
    ensure!(objects::valid_name(key), "invalid archive object name");
    let mut url = collection_url(collection)?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("WebDAV collection cannot contain objects"))?
        .pop_if_empty()
        .push(key);
    Ok(url.into())
}

pub struct WebDav {
    collection: String,
    client: ureq::Agent,
    authorization: Option<String>,
}

impl WebDav {
    pub fn new(collection: &str, credentials: Option<(&str, &str)>) -> Result<Self> {
        Ok(Self {
            collection: collection_url(collection)?.into(),
            client: ureq::AgentBuilder::new()
                .redirects(0)
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(120))
                .timeout_write(Duration::from_secs(120))
                .build(),
            authorization: credentials
                .map(|(user, pass)| format!("Basic {}", STANDARD.encode(format!("{user}:{pass}")))),
        })
    }

    fn request(&self, method: &str, key: &str) -> Result<ureq::Request> {
        let req = self
            .client
            .request(method, &object_url(&self.collection, key)?);
        Ok(match &self.authorization {
            Some(header) => req.set("Authorization", header),
            None => req,
        })
    }

    pub fn upload(&self, file: &mut File, digest: &str, name: &str) -> Result<String> {
        let key = objects::name(name);
        file.rewind()?;
        let size = file.metadata()?.len();
        let response = self
            .request("PUT", &key)?
            .set("Content-Length", &size.to_string())
            .send(file)?;
        ensure!(
            (200..300).contains(&response.status()),
            "PUT returned {}",
            response.status()
        );
        if let Err(error) = self.read_to(&key, digest, size, &mut io::sink()) {
            let _ = self.remove(&key);
            return Err(error.context("verifying archived object"));
        }
        Ok(key)
    }

    pub fn read_to(
        &self,
        key: &str,
        digest: &str,
        size: u64,
        output: &mut impl Write,
    ) -> Result<()> {
        let response = self.request("GET", key)?.call()?;
        ensure!(
            response.status() == 200,
            "GET returned {}",
            response.status()
        );
        let mut input = response.into_reader();
        let mut buffer = vec![0; 128 * 1024];
        let mut hash = Sha256::new();
        let mut received = 0_u64;
        loop {
            let n = input.read(&mut buffer).context("reading WebDAV object")?;
            if n == 0 {
                break;
            }
            received += n as u64;
            ensure!(received <= size, "archive exceeds expected size");
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?;
        }
        ensure!(received == size, "archive length mismatch");
        ensure!(
            format!("{:x}", hash.finalize()) == digest,
            "archive checksum mismatch"
        );
        Ok(())
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        match self.request("DELETE", key)?.call() {
            Ok(response) => {
                ensure!(
                    (200..300).contains(&response.status()),
                    "DELETE returned {}",
                    response.status()
                );
                Ok(())
            }
            Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_names_are_one_encoded_url_segment() -> Result<()> {
        let key = objects::name("旅行 #?%.jpg");
        let url = object_url("https://example.com/files%20here/", &key)?;
        assert!(url.starts_with("https://example.com/files%20here/"));
        assert!(url.ends_with("%E6%97%85%E8%A1%8C%20%23%3F%25.jpg"));
        assert_eq!(Url::parse(&url)?.query(), None);
        assert_eq!(Url::parse(&url)?.fragment(), None);
        for key in ["../outside", "file/child", "plain-name"] {
            assert!(object_url("https://example.com/dav", key).is_err());
        }
        for collection in [
            "file:///tmp",
            "https://user:secret@example.com/dav",
            "https://example.com/dav?x=1",
        ] {
            assert!(object_url(collection, &key).is_err());
        }
        Ok(())
    }
}
