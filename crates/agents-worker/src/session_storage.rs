use serde::{Deserialize, Serialize, de::DeserializeOwned};
use worker::{Error, Result, Storage};

const CHUNK_BYTES: usize = 256 * 1024;
const BATCH_KEYS: usize = 128;
// Stored Vec<u8> values expand into JS arrays; cap bytes as well as key count.
const BATCH_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    // Serde's buffered tagged-enum decoder otherwise exposes arbitrary-precision
    // JSON numbers as private maps to nested f64 fields. Keep exact JSON Values
    // until the typed decode, including large integers in arbitrary user data.
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    serde_json::from_value(value).map_err(Error::from)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Index {
    generation: String,
    chunks: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bytes: Option<usize>,
}

struct Chunk {
    entry: usize,
    offset: usize,
    key: String,
    bytes: Option<usize>,
}

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}

fn decode<T: DeserializeOwned>(map: &js_sys::Map, key: &str) -> Result<Option<T>> {
    let value = map.get(&key.into());
    if value.is_undefined() {
        return Ok(None);
    }
    serde_wasm_bindgen::from_value(value)
        .map(Some)
        .map_err(|_| fail("session_storage_corrupt"))
}

fn plan(keys: &[String], indices: &[Option<Index>]) -> Result<Vec<Vec<Chunk>>> {
    let mut batches = vec![];
    let mut batch = vec![];
    let mut bytes = 0;
    for (entry, (key, index)) in keys.iter().zip(indices).enumerate() {
        let Some(index) = index else { continue };
        if index
            .bytes
            .is_some_and(|bytes| bytes.div_ceil(CHUNK_BYTES) != index.chunks)
        {
            return Err(fail("session_storage_corrupt"));
        }
        for chunk in 0..index.chunks {
            let offset = chunk
                .checked_mul(CHUNK_BYTES)
                .ok_or_else(|| fail("session_storage_corrupt"))?;
            let exact = index.bytes.map(|total| (total - offset).min(CHUNK_BYTES));
            let size = exact.unwrap_or(CHUNK_BYTES);
            if batch.len() == BATCH_KEYS || bytes + size > BATCH_BYTES {
                batches.push(std::mem::take(&mut batch));
                bytes = 0;
            }
            batch.push(Chunk {
                entry,
                offset,
                key: format!("{key}:{}:{chunk}", index.generation),
                bytes: exact,
            });
            bytes += size;
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    Ok(batches)
}

fn apply_batch(
    batch: &[Chunk],
    entries: &mut [Option<Vec<u8>>],
    mut get: impl FnMut(&str) -> Result<Option<Vec<u8>>>,
) -> Result<()> {
    for chunk in batch {
        let bytes = get(&chunk.key)?.ok_or_else(|| fail("session_chunk_missing"))?;
        if bytes.len() > CHUNK_BYTES || chunk.bytes.is_some_and(|len| len != bytes.len()) {
            return Err(fail("session_storage_corrupt"));
        }
        let entry = entries[chunk.entry]
            .as_mut()
            .ok_or_else(|| fail("session_storage_corrupt"))?;
        if entry.len() != chunk.offset {
            return Err(fail("session_storage_corrupt"));
        }
        entry.extend(bytes);
    }
    Ok(())
}

pub async fn read_many(storage: &Storage, keys: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
    let mut indices = Vec::with_capacity(keys.len());
    for batch in keys.chunks(BATCH_KEYS) {
        let map = storage.get_multiple(batch.to_vec()).await?;
        for key in batch {
            indices.push(decode::<Index>(&map, key)?);
        }
    }
    let mut entries = indices
        .iter()
        .map(|index| index.as_ref().map(|_| vec![]))
        .collect::<Vec<_>>();
    for batch in plan(keys, &indices)? {
        let map = storage
            .get_multiple(batch.iter().map(|chunk| chunk.key.clone()).collect())
            .await?;
        apply_batch(&batch, &mut entries, |key| decode::<Vec<u8>>(&map, key))?;
    }
    Ok(entries)
}

pub async fn read_chunks(storage: &Storage, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(read_many(storage, &[key.into()]).await?.pop().flatten())
}

// Publish the generation index last, retaining the previous generation until then.
pub async fn write_chunks(storage: &Storage, key: &str, bytes: &[u8]) -> Result<()> {
    let generation = uuid::Uuid::new_v4().simple().to_string();
    let chunks = bytes.chunks(CHUNK_BYTES).collect::<Vec<_>>();
    for (index, chunk) in chunks.iter().enumerate() {
        storage
            .put(&format!("{key}:{generation}:{index}"), &chunk.to_vec())
            .await?;
    }
    let old = storage.get::<Index>(key).await?;
    storage
        .put(
            key,
            Index {
                generation,
                chunks: chunks.len(),
                bytes: Some(bytes.len()),
            },
        )
        .await?;
    if let Some(old) = old {
        for index in 0..old.chunks {
            storage
                .delete(&format!("{key}:{}:{index}", old.generation))
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn fixture(
        lengths: &[usize],
        legacy: bool,
    ) -> (
        Vec<String>,
        Vec<Option<Index>>,
        HashMap<String, Vec<u8>>,
        Vec<Option<Vec<u8>>>,
    ) {
        let keys = (0..lengths.len())
            .map(|i| format!("journal:{i}"))
            .collect::<Vec<_>>();
        let mut indices = vec![];
        let mut chunks = HashMap::new();
        let mut expected = vec![];
        for (i, length) in lengths.iter().enumerate().rev() {
            let bytes = (0..*length)
                .map(|offset| (i.wrapping_add(offset) % 251) as u8)
                .collect::<Vec<_>>();
            for (n, chunk) in bytes.chunks(CHUNK_BYTES).enumerate().rev() {
                chunks.insert(format!("journal:{i}:generation:{n}"), chunk.to_vec());
            }
            indices.push(Some(Index {
                generation: "generation".into(),
                chunks: bytes.len().div_ceil(CHUNK_BYTES),
                bytes: if legacy { None } else { Some(bytes.len()) },
            }));
            expected.push(Some(bytes));
        }
        indices.reverse();
        expected.reverse();
        (keys, indices, chunks, expected)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn reconstructs_ordered_entries_from_unordered_chunk_maps(lengths in prop::collection::vec(0usize..CHUNK_BYTES*2+17, 0..9), legacy in any::<bool>()) {
            let (keys, indices, chunks, expected) = fixture(&lengths, legacy);
            let mut actual = indices.iter().map(|_| Some(vec![])).collect::<Vec<_>>();
            for batch in plan(&keys,&indices).unwrap() {
                prop_assert!(batch.len()<=BATCH_KEYS);
                prop_assert!(batch.iter().map(|chunk|chunk.bytes.unwrap_or(CHUNK_BYTES)).sum::<usize>()<=BATCH_BYTES);
                apply_batch(&batch,&mut actual,|key|Ok(chunks.get(key).cloned())).unwrap();
            }
            prop_assert_eq!(actual,expected);
        }

        #[test]
        fn any_missing_chunk_rejects_the_whole_read(lengths in prop::collection::vec(1usize..CHUNK_BYTES*2+17, 1..9), victim in any::<usize>()) {
            let (keys, indices, mut chunks, _) = fixture(&lengths,false);
            let mut chunk_keys=chunks.keys().cloned().collect::<Vec<_>>();
            chunk_keys.sort();
            chunks.remove(&chunk_keys[victim%chunk_keys.len()]);
            let mut actual=indices.iter().map(|_|Some(vec![])).collect::<Vec<_>>();
            let mut failed=false;
            for batch in plan(&keys,&indices).unwrap() {
                if apply_batch(&batch,&mut actual,|key|Ok(chunks.get(key).cloned())).is_err() {
                    failed=true;
                    break;
                }
            }
            prop_assert!(failed);
        }
    }

    #[test]
    fn new_and_legacy_indices_bound_large_value_batches() {
        for legacy in [false, true] {
            let (keys, indices, _, _) = fixture(&vec![32 * 1024; 258], legacy);
            assert_eq!(
                plan(&keys, &indices).unwrap().len(),
                if legacy { 17 } else { 3 }
            );
        }
        let old: Index = serde_json::from_str(r#"{"generation":"old","chunks":1}"#).unwrap();
        assert_eq!(old.bytes, None);
    }

    #[test]
    fn missing_or_corrupted_chunks_never_return_partial_history() {
        let (keys, mut indices, mut chunks, _) = fixture(&[CHUNK_BYTES + 1], false);
        let batches = plan(&keys, &indices).unwrap();
        chunks.remove("journal:0:generation:1");
        assert!(
            apply_batch(&batches[0], &mut [Some(vec![])], |key| Ok(chunks
                .get(key)
                .cloned()))
            .is_err()
        );
        chunks.insert("journal:0:generation:1".into(), vec![1, 2]);
        assert!(
            apply_batch(&batches[0], &mut [Some(vec![])], |key| Ok(chunks
                .get(key)
                .cloned()))
            .is_err()
        );
        indices[0].as_mut().unwrap().bytes = Some(1);
        assert!(plan(&keys, &indices).is_err());
        assert!(plan(&keys, &[None]).unwrap().is_empty());
    }
}
