use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub id: Uuid,
    #[serde(
        serialize_with = "serialize_timestamp",
        deserialize_with = "deserialize_timestamp"
    )]
    pub created_at: OffsetDateTime,
    pub label: String,
    pub files: HashMap<PathBuf, Vec<ChunkRef>>,
    pub small_blocks: HashMap<PathBuf, Vec<ChunkRef>>,
}

#[derive(Debug)]
pub struct ChunkRef(pub blake3::Hash);

impl Serialize for ChunkRef
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_hex())
    }
}

impl<'de> Deserialize<'de> for ChunkRef
{
    fn deserialize<D>(deserializer: D) -> Result<ChunkRef, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(ChunkRef(blake3::Hash::from_hex(&s).unwrap()))
    }
}

fn serialize_timestamp<S>(dt: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_i64(dt.unix_timestamp())
}

fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
where
    D: Deserializer<'de>,
{
    let ts = i64::deserialize(deserializer)?;
    Ok(OffsetDateTime::from_unix_timestamp(ts).unwrap()) // TODO: fix this
}
