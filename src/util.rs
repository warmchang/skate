pub(crate) mod linux;

use crate::exec::ShellExec;
use crate::supported_resources::SupportedResources;
use anyhow::anyhow;
use base64::engine::general_purpose;
use base64::Engine;
use chrono::{DateTime, Local};
use deunicode::deunicode_char;
use fs2::FileExt;
use itertools::Itertools;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::Metadata;
use log::info;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::Path;
use strum_macros::{Display, EnumString};

pub const CHECKBOX_EMOJI: char = '✔';
pub const CROSS_EMOJI: char = '✖';
#[allow(unused)]
pub const EQUAL_EMOJI: char = '~';
#[allow(unused)]
pub const INFO_EMOJI: &str = "[i]";

pub fn slugify<S: AsRef<str>>(s: S) -> String {
    _slugify(s.as_ref())
}

#[derive(EnumString, Display)]
#[strum(serialize_all = "lowercase", prefix = "skate.io/")]
pub enum SkateLabels {
    Name,
    Namespace,
    Hash,
    Replica,
    Arch,
    Daemonset,
    Deployment,
    Nodename,
    Hostname,
    Cronjob,
}

#[doc(hidden)]
#[cfg(target_family = "wasm")]
#[wasm_bindgen(js_name = slugify)]
pub fn slugify_owned(s: String) -> String {
    _slugify(s.as_ref())
}

// avoid unnecessary monomorphizations
fn _slugify(s: &str) -> String {
    let mut slug: Vec<u8> = Vec::with_capacity(s.len());
    // Starts with true to avoid leading -
    let mut prev_is_dash = true;
    {
        let mut push_char = |x: u8| {
            match x {
                b'a'..=b'z' | b'0'..=b'9' => {
                    prev_is_dash = false;
                    slug.push(x);
                }
                b'A'..=b'Z' => {
                    prev_is_dash = false;
                    // Manual lowercasing as Rust to_lowercase() is unicode
                    // aware and therefore much slower
                    slug.push(x - b'A' + b'a');
                }
                _ => {
                    if !prev_is_dash {
                        slug.push(b'-');
                        prev_is_dash = true;
                    }
                }
            }
        };

        for c in s.chars() {
            if c.is_ascii() {
                push_char(c as u8);
            } else {
                for &cx in deunicode_char(c).unwrap_or("-").as_bytes() {
                    push_char(cx);
                }
            }
        }
    }

    // It's not really unsafe in practice, we know we have ASCII
    let mut string = unsafe { String::from_utf8_unchecked(slug) };
    if string.ends_with('-') {
        string.pop();
    }
    // We likely reserved more space than needed.
    string.shrink_to_fit();
    string
}

pub fn hash_string<T>(obj: T) -> String
where
    T: Hash,
{
    let mut hasher = DefaultHasher::new();
    obj.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

pub fn calc_k8s_resource_hash(obj: (impl Metadata<Ty = ObjectMeta> + Serialize + Clone)) -> String {
    let mut obj = obj.clone();

    let mut labels = obj.metadata().labels.clone().unwrap_or_default();
    // remove stuff that changes regardless
    labels.remove(&SkateLabels::Hash.to_string());
    obj.metadata_mut().generation = None;

    // sort labels
    labels = labels
        .into_iter()
        .sorted_by_key(|(k, _v)| k.clone())
        .collect();
    obj.metadata_mut().labels = Option::from(labels);

    let mut annotations = obj.metadata().annotations.clone().unwrap_or_default();

    // sort annotations
    annotations = annotations
        .into_iter()
        .sorted_by_key(|(k, _v)| k.clone())
        .collect();
    obj.metadata_mut().annotations = Option::from(annotations);

    let serialized = serde_yaml::to_string(&obj).unwrap();

    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

#[derive(Serialize, Default, Deserialize, Debug, Clone, Eq, Hash, PartialEq)]
pub struct NamespacedName {
    pub name: String,
    pub namespace: String,
}

impl From<&str> for NamespacedName {
    fn from(s: &str) -> Self {
        let parts: Vec<_> = s.split('.').collect();
        Self {
            name: parts.first().unwrap_or(&"").to_string(),
            namespace: parts.last().unwrap_or(&"").to_string(),
        }
    }
}

impl Display for NamespacedName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(format!("{}.{}", self.name, self.namespace).as_str())
    }
}

impl NamespacedName {
    pub fn new(name: &str, namespace: &str) -> Self {
        NamespacedName {
            name: name.to_string(),
            namespace: namespace.to_string(),
        }
    }
}

pub trait GetSkateLabels {
    fn namespaced_name(&self) -> NamespacedName;
    fn hash(&self) -> String;
}

pub fn get_label_value(
    labels: &Option<std::collections::BTreeMap<String, String>>,
    key: &str,
) -> Option<String> {
    labels.as_ref().and_then(|l| l.get(key).cloned())
}

pub fn get_skate_label_value(
    labels: &Option<std::collections::BTreeMap<String, String>>,
    key: &SkateLabels,
) -> Option<String> {
    labels
        .as_ref()
        .and_then(|l| l.get(&key.to_string()).cloned())
}

impl GetSkateLabels for ObjectMeta {
    fn namespaced_name(&self) -> NamespacedName {
        let name = get_skate_label_value(&self.labels, &SkateLabels::Name);
        let ns = get_skate_label_value(&self.labels, &SkateLabels::Namespace);

        if name.is_none() {
            panic!("metadata missing skate.io/name label")
        }

        if ns.is_none() {
            panic!("metadata missing skate.io/namespace label")
        }

        NamespacedName::new(&name.unwrap(), &ns.unwrap())
    }

    fn hash(&self) -> String {
        get_skate_label_value(&self.labels, &SkateLabels::Hash).unwrap_or("".to_string())
    }
}

// returns name, namespace
pub fn metadata_name(obj: &impl Metadata<Ty = ObjectMeta>) -> NamespacedName {
    let m = obj.metadata();
    m.namespaced_name()
}

// hash_k8s_resource hashes a k8s resource and adds the hash to the labels, also returning it
pub fn hash_k8s_resource(obj: &mut (impl Metadata<Ty = ObjectMeta> + Serialize + Clone)) -> String {
    let hash = calc_k8s_resource_hash(obj.clone());

    let mut labels = obj.metadata().labels.clone().unwrap_or_default();
    labels.insert(SkateLabels::Hash.to_string(), hash.clone());
    obj.metadata_mut().labels = Option::from(labels);
    hash
}

// age returns the age of a resource in a human-readable format, with only the first segment of resolution (eg 2d1h4m  becomes 2d)
pub fn age(date_time: DateTime<Local>) -> String {
    match Local::now().signed_duration_since(date_time).to_std() {
        Ok(duration) => {
            if duration.as_secs() < 60 {
                return format!("{}s", duration.as_secs());
            }
            let minutes = duration.as_secs() / 60;
            if minutes < 60 {
                return format!("{}m", minutes);
            }
            let hours = duration.as_secs() / (60 * 60);
            if hours < 24 {
                return format!("{}h", hours);
            }

            let days = duration.as_secs() / (60 * 60 * 24);
            format!("{}d", days)
        }
        Err(_) => "".to_string(),
    }
}

pub fn spawn_orphan_process<I, S>(cmd: &str, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // The fact that we don't have a `?` or `unrwap` here is intentional
    // This disowns the process, which is what we want.
    let _ = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
pub fn lock_file<T>(
    file: &str,
    cb: Box<dyn FnOnce() -> Result<T, Box<dyn Error>>>,
) -> Result<T, Box<dyn Error>> {
    let lock_path = Path::new(file);
    let lock_file =
        File::create(lock_path).map_err(|e| anyhow!("failed to create/open lock file: {}", e))?;
    info!("waiting for lock on {}", lock_path.display());
    lock_file.lock_exclusive()?;
    info!("locked {}", lock_path.display());
    let result = cb();
    lock_file.unlock()?;
    info!("unlocked {}", lock_path.display());
    result
}

fn write_manifest_to_file(manifest: &str) -> Result<String, Box<dyn Error>> {
    let file_path = format!("/tmp/skate-{}.yaml", hash_string(manifest));
    let mut file = File::create(file_path.clone()).expect("failed to open file for manifests");
    file.write_all(manifest.as_ref())
        .expect("failed to write manifest to file");
    Ok(file_path)
}

pub fn apply_play(
    execer: &Box<dyn ShellExec>,
    object: &SupportedResources,
) -> Result<(), Box<dyn Error>> {
    let file_path = write_manifest_to_file(&serde_yaml::to_string(object)?)?;

    let mut args = vec!["play", "kube", &file_path, "--start"];
    if !object.host_network() {
        args.push("--network=skate")
    }

    let result = execer.exec("podman", &args, None)?;

    if !result.is_empty() {
        println!("{}", result);
    }
    Ok(())
}

pub fn version(long: bool) -> String {
    let tag = crate::build::TAG;
    let short_version = if tag.is_empty() {
        crate::build::COMMIT_HASH
    } else {
        tag
    };

    if !long {
        return short_version.to_string();
    }
    format!(
        r#"{}
branch:{}
commit_hash:{}
build_time:{}"#,
        short_version,
        crate::build::BRANCH,
        crate::build::COMMIT_HASH,
        crate::build::BUILD_TIME
    )
}

pub fn tabled_display_option<T>(o: &Option<T>) -> String
where
    T: Display,
{
    match o {
        Some(s) => format!("{}", s),
        None => "-".to_string(),
    }
}

pub fn transfer_file_cmd(contents: &str, remote_path: &str) -> String {
    format!(
        "sudo bash -c -eu 'echo {}| base64 --decode > {}'",
        general_purpose::STANDARD.encode(contents),
        remote_path
    )
}

pub static RE_CIDR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^([0-9]{1,3}\.){3}[0-9]{1,3}($|/(16|24))").unwrap());
pub static RE_IP: Lazy<Regex> = Lazy::new(|| Regex::new(r"^([0-9]{1,3}\.){3}[0-9]{1,3}$").unwrap());
pub static RE_HOST_SEGMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[0-9a-z\-_]+$").unwrap());
pub static RE_HOSTNAME: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(([a-zA-Z0-9]|[a-zA-Z0-9][a-zA-Z0-9\-]*[a-zA-Z0-9])\.)*([A-Za-z0-9]|[A-Za-z0-9][A-Za-z0-9\-]*[A-Za-z0-9])$").unwrap()
});

pub enum ImageTagFormat {
    None,
    Digest(String),
    Tag(String),
}
impl ImageTagFormat {
    pub fn to_suffix(&self) -> String {
        match self {
            ImageTagFormat::None => "".to_string(),
            ImageTagFormat::Digest(digest) => format!("@{digest}"),
            ImageTagFormat::Tag(tag) => format!(":{tag}"),
        }
    }
}
/// Splits a container image string into its base image and tag/digest.
pub fn split_container_image(image: &str) -> (String, ImageTagFormat) {
    if image.contains('@') {
        let (image, digest) = image.split_once('@').unwrap();
        let digest = digest.to_string();
        (image.to_string(), ImageTagFormat::Digest(digest))
    } else if image.contains(':') {
        let (image, tag) = image.split_once(':').unwrap();
        let tag = tag.to_string();
        (image.to_string(), ImageTagFormat::Tag(tag))
    } else {
        (image.to_string(), ImageTagFormat::None)
    }
}

pub trait VecInto<D> {
    #[allow(unused)]
    fn vec_into(self) -> Vec<D>;
}

impl<E, D> VecInto<D> for Vec<E>
where
    D: From<E>,
{
    fn vec_into(self) -> Vec<D> {
        self.into_iter().map(std::convert::Into::into).collect()
    }
}

pub trait TryVecInto<D> {
    type Error;
    fn try_vec_into(self) -> Result<Vec<D>, Self::Error>;
}

impl<E, D, E2> TryVecInto<D> for Vec<E>
where
    D: TryFrom<E, Error = E2>,
{
    type Error = E2;
    fn try_vec_into(self) -> Result<Vec<D>, Self::Error> {
        self.into_iter().map(TryFrom::try_from).collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::util::{age, get_label_value, SkateLabels};
    use chrono::{Duration, Local};

    #[test]
    fn test_age() {
        let conditions = &[
            (Local::now(), "0s"),
            (Local::now() - Duration::seconds(20), "20s"),
            (Local::now() - Duration::minutes(20), "20m"),
            (Local::now() - Duration::minutes(20 * 60), "20h"),
            (Local::now() - Duration::minutes(20 * 60 * 24), "20d"),
        ];

        for (input, expect) in conditions {
            let output = age(*input);
            assert_eq!(output, *expect, "input: {}", input);
        }
    }

    #[test]
    fn test_get_label_value() {
        let labels = std::collections::BTreeMap::from([
            ("skate.io/name".to_string(), "test".to_string()),
            ("skate.io/namespace".to_string(), "default".to_string()),
        ]);
        let name = get_label_value(&Some(labels), "skate.io/name");
        assert_eq!(name, Some("test".to_string()));
    }

    #[test]
    fn test_skate_labels_serialize() {
        assert_eq!("skate.io/name", SkateLabels::Name.to_string());
        assert_eq!("skate.io/daemonset", SkateLabels::Daemonset.to_string());
    }
}
