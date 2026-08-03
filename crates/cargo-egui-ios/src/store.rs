//! Publish built plugins to an appstore plugin store.
//!
//! The server records what the publisher states rather than parsing the module, so
//! identity and permissions are read out of `manifest.toml` here and sent as query
//! parameters alongside the raw `.wasm` body.

use anyhow::{Context, Result, bail};

/// Where to publish and the admin credential, from flags or the environment.
#[derive(Clone, Debug)]
pub struct StoreTarget {
    pub url: String,
    pub key: String,
}

impl StoreTarget {
    /// `--store-url` / `--store-key`, else `AS_URL` / `AS_KEY`. `None` when unset,
    /// which is how publishing stays opt-in.
    pub fn resolve(url: Option<String>, key: Option<String>) -> Option<StoreTarget> {
        let url = url.or_else(|| std::env::var("AS_URL").ok())?;
        let key = key.or_else(|| std::env::var("AS_KEY").ok())?;
        let url = url.trim().trim_end_matches('/').to_owned();
        let key = key.trim().to_owned();
        (!url.is_empty() && !key.is_empty()).then_some(StoreTarget { url, key })
    }
}

/// Percent-encode a query-parameter value.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn str_field(v: &toml::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or_default().to_owned()
}

/// Upload `plugin.wasm` and then `manifest.toml` for one plugin.
pub fn publish(target: &StoreTarget, id: &str, manifest_toml: &str, wasm: &[u8], notes: &str) -> Result<()> {
    let manifest: toml::Value =
        toml::from_str(manifest_toml).context("parsing manifest.toml for publish")?;
    let version = str_field(&manifest, "version");
    if version.is_empty() {
        bail!("manifest.toml for {id} has no `version`; the store needs one");
    }
    let name = str_field(&manifest, "name");
    let description = str_field(&manifest, "description");
    let abi = manifest.get("abi_version").and_then(|v| v.as_integer()).unwrap_or(1);
    // permissions = ["net", "haptic"] -> "net,haptic"
    let permissions = manifest
        .get("permissions")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")
        })
        .unwrap_or_default();

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(300))
        .build();

    let query = format!(
        "version={}&name={}&description={}&permissions={}&abi_version={}&notes={}",
        enc(&version),
        enc(&name),
        enc(&description),
        enc(&permissions),
        abi,
        enc(notes),
    );
    let url = format!("{}/plugins/{}/upload?{}", target.url, enc(id), query);
    send(&agent, &url, &target.key, wasm).with_context(|| format!("publishing {id} to the store"))?;

    let manifest_url = format!("{}/plugins/{}/manifest", target.url, enc(id));
    send(&agent, &manifest_url, &target.key, manifest_toml.as_bytes())
        .with_context(|| format!("uploading manifest for {id}"))?;

    println!("published {id} v{version} to {}", target.url);
    Ok(())
}

fn send(agent: &ureq::Agent, url: &str, key: &str, body: &[u8]) -> Result<()> {
    match agent
        .post(url)
        .set("x-api-key", key)
        .set("Accept", "application/json")
        .set("Content-Type", "application/octet-stream")
        .send_bytes(body)
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            bail!("HTTP {code}: {}", detail.trim())
        }
        Err(e) => bail!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values() {
        assert_eq!(enc("0.1.0"), "0.1.0");
        assert_eq!(enc("net,haptic"), "net%2Chaptic");
        assert_eq!(enc("a b"), "a%20b");
    }

    #[test]
    fn target_needs_both_url_and_key() {
        assert!(StoreTarget::resolve(Some("https://x".into()), Some("k".into())).is_some());
        assert!(StoreTarget::resolve(Some("https://x".into()), Some("".into())).is_none());
        assert!(StoreTarget::resolve(Some("".into()), Some("k".into())).is_none());
    }

    #[test]
    fn target_trims_trailing_slash() {
        let t = StoreTarget::resolve(Some("https://x/ ".into()), Some(" k ".into())).unwrap();
        assert_eq!(t.url, "https://x");
        assert_eq!(t.key, "k");
    }
}
