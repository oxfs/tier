//! WebDAV object access for an existing collection, including OpenList.
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    time::Duration,
};
use uuid::Uuid;

pub struct Archive {
    collection: String,
    client: ureq::Agent,
    authorization: Option<String>,
}

impl Archive {
    pub fn new(collection: &str, credentials: Option<(&str, &str)>) -> Result<Self> {
        ensure!(
            collection.starts_with("https://") || collection.starts_with("http://"),
            "WebDAV URL must use HTTP(S)"
        );
        ensure!(
            !collection.contains(['?', '#', '@']),
            "use a plain collection URL without embedded credentials"
        );
        Ok(Self {
            collection: collection.trim_end_matches('/').to_owned(),
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
        ensure!(Uuid::parse_str(key).is_ok(), "invalid archive object ID");
        let req = self
            .client
            .request(method, &format!("{}/{key}", self.collection));
        Ok(match &self.authorization {
            Some(header) => req.set("Authorization", header),
            None => req,
        })
    }

    pub fn upload(&self, file: &mut File, digest: &str) -> Result<String> {
        let key = Uuid::new_v4().to_string();
        file.rewind()?;
        let response = self
            .request("PUT", &key)?
            .set("Content-Length", &file.metadata()?.len().to_string())
            .send(file)?;
        ensure!(
            (200..300).contains(&response.status()),
            "PUT returned {}",
            response.status()
        );
        if let Err(error) = self.read_to(&key, digest, &mut io::sink()) {
            let _ = self.remove(&key);
            return Err(error.context("verifying archived object"));
        }
        Ok(key)
    }

    pub fn read_to(&self, key: &str, digest: &str, output: &mut impl Write) -> Result<()> {
        let response = self.request("GET", key)?.call()?;
        ensure!(
            response.status() == 200,
            "GET returned {}",
            response.status()
        );
        let mut input = response.into_reader();
        let mut buffer = vec![0; 128 * 1024];
        let mut hash = Sha256::new();
        loop {
            let n = input.read(&mut buffer).context("reading WebDAV object")?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?;
        }
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
