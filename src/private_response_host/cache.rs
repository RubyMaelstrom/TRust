//! CacheStorage name lookups/open/delete and Cache-object lifetimes are one
//! native transaction. SW 92aba3be #cache-storage-open / #cache-storage-delete:
//! deleting a name leaves referenced Cache objects functional; after their
//! last reference disappears, the unnamed request/response list can be freed.
use super::*;

#[derive(serde::Serialize, serde::Deserialize)]
struct Names {
    next: u64,
    entries: Vec<(String, String)>,
}
impl Default for Names {
    fn default() -> Self {
        Self {
            next: 1,
            entries: Vec::new(),
        }
    }
}
fn names(storage: &HashMap<String, HashMap<String, String>>, owner: &str) -> Names {
    storage
        .get(&format!("cache-storage-names:{owner}"))
        .and_then(|entries| entries.get("map"))
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default()
}
fn save_names(storage: &mut HashMap<String, HashMap<String, String>>, owner: &str, names: &Names) {
    storage
        .entry(format!("cache-storage-names:{owner}"))
        .or_default()
        .insert("map".into(), serde_json::to_string(names).unwrap());
}

pub(super) struct Lease {
    storage: Weak<StorageMap>,
    profile: Arc<Profile>,
    owner: String,
    id: String,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let key = (
            format!("cache-storage-data:{}", self.owner),
            self.id.clone(),
        );
        {
            let mut state = self.profile.state.lock().unwrap();
            if let Some(count) = state.cache_users.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    state.cache_users.remove(&key);
                }
            }
        }
        if let Some(storage) = self.storage.upgrade() {
            cleanup(
                &mut storage.lock().unwrap(),
                &self.profile,
                &self.owner,
                &self.id,
            );
        }
    }
}

fn cleanup(
    storage: &mut HashMap<String, HashMap<String, String>>,
    profile: &Profile,
    owner: &str,
    id: &str,
) {
    if names(storage, owner)
        .entries
        .iter()
        .any(|(_, candidate)| candidate == id)
    {
        return;
    }
    let bucket = format!("cache-storage-data:{owner}");
    let key = (bucket.clone(), id.to_string());
    let mut state = profile.state.lock().unwrap();
    if state.cache_users.get(&key).copied().unwrap_or(0) != 0 {
        return;
    }
    if let Some(entries) = storage.get_mut(&bucket) {
        entries.remove(id);
    }
    state.cached.remove(&key);
}

impl Agent {
    // Call while holding the shed lock used by name lookup. A simultaneous
    // delete cannot free the list between finding its id and retaining it.
    fn cache_lease(
        &mut self,
        storage: &crate::js::WebStorage,
        profile: &Arc<Profile>,
        owner: &str,
        id: &str,
    ) -> serde_json::Value {
        self.next += 1;
        let key = (format!("cache-storage-data:{owner}"), id.to_string());
        *profile
            .state
            .lock()
            .unwrap()
            .cache_users
            .entry(key)
            .or_default() += 1;
        self.cache_leases.insert(
            self.next,
            Lease {
                storage: Arc::downgrade(storage),
                profile: profile.clone(),
                owner: owner.into(),
                id: id.into(),
            },
        );
        serde_json::json!({"id": id, "lease": self.next})
    }
}

fn change(
    agent: &mut Agent,
    shed: &crate::js::WebStorage,
    owner: &str,
    op: &str,
    name: Option<&str>,
) -> Result<serde_json::Value, &'static str> {
    let profile = profile(shed);
    let mut storage = shed.lock().unwrap();
    let mut names = names(&storage, owner);
    match op {
        "cache-open" => {
            let name = name.ok_or("TypeError")?;
            let id = if let Some((_, id)) = names
                .entries
                .iter()
                .find(|(candidate, _)| candidate == name)
            {
                id.clone()
            } else {
                if names.entries.len() >= 1024 {
                    return Err("QuotaExceededError");
                }
                let id = names.next.to_string();
                names.next = names.next.checked_add(1).ok_or("QuotaExceededError")?;
                names.entries.push((name.to_string(), id.clone()));
                let bucket = format!("cache-storage-data:{owner}");
                let serialized = storage
                    .get(&bucket)
                    .into_iter()
                    .flat_map(|entries| entries.values())
                    .map(String::len)
                    .sum::<usize>();
                let private = profile
                    .state
                    .lock()
                    .unwrap()
                    .cached
                    .iter()
                    .filter(|((key, _), _)| *key == bucket)
                    .flat_map(|(_, payloads)| payloads)
                    .map(|payload| payload.cache_size())
                    .sum::<usize>();
                if serialized
                    .saturating_add(private)
                    .saturating_add(serde_json::to_string(&names).unwrap().len())
                    > 64 * 1024 * 1024
                {
                    return Err("QuotaExceededError");
                }
                save_names(&mut storage, owner, &names);
                id
            };
            Ok(agent.cache_lease(shed, &profile, owner, &id))
        }
        "cache-list" => {
            let mut result = Vec::new();
            for (candidate, id) in names.entries {
                if name.is_none_or(|name| name == candidate) {
                    result.push(agent.cache_lease(shed, &profile, owner, &id));
                }
            }
            Ok(serde_json::Value::Array(result))
        }
        "cache-delete" => {
            let name = name.ok_or("TypeError")?;
            let Some(index) = names
                .entries
                .iter()
                .position(|(candidate, _)| candidate == name)
            else {
                return Ok(false.into());
            };
            let (_, id) = names.entries.remove(index);
            save_names(&mut storage, owner, &names);
            cleanup(&mut storage, &profile, owner, &id);
            Ok(true.into())
        }
        _ => Err("TypeError"),
    }
}

pub(super) fn operation<'s>(
    ctx: &mut Ctx<'s>,
    op: &str,
    args: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    if op == "cache-release" {
        let lease = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.) as u64;
        ctx.host_mut::<HostState>()
            .unwrap()
            .private_responses
            .cache_leases
            .remove(&lease);
        return Ok(Value::Undefined);
    }
    let input = host_arg_string(ctx, args, 1);
    let input: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    // As with __storage_get/cas, the platform block supplies its object's
    // immutable owning storage key, independently of the current caller.
    let Some(owner) = input["origin"].as_str().filter(|key| *key != "null") else {
        return Ok(ctx.string("{\"error\":\"SecurityError\"}".into()));
    };
    let state = ctx.host_mut::<HostState>().unwrap();
    let result = change(
        &mut state.private_responses,
        &state.storage,
        owner,
        op,
        input["name"].as_str(),
    );
    Ok(ctx.string(match result {
        Ok(value) => value.to_string(),
        Err(error) => serde_json::json!({"error":error}).to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deleting_a_name_preserves_cache_objects_then_frees_the_unnamed_list() {
        let shed: crate::js::WebStorage = Arc::default();
        let mut a = Agent::default();
        let mut b = Agent::default();
        let owner = "https://cache.test";
        let first = change(&mut a, &shed, owner, "cache-open", Some("v1")).unwrap();
        let id = first["id"].as_str().unwrap().to_string();
        let matched = change(&mut b, &shed, owner, "cache-list", None).unwrap();
        assert_eq!(matched[0]["id"], id);
        shed.lock()
            .unwrap()
            .entry(format!("cache-storage-data:{owner}"))
            .or_default()
            .insert(id.clone(), "[]".into());
        assert_eq!(
            change(&mut a, &shed, owner, "cache-delete", Some("v1")).unwrap(),
            true
        );
        drop(a);
        assert!(shed.lock().unwrap()[&format!("cache-storage-data:{owner}")].contains_key(&id));
        let replacement = change(&mut b, &shed, owner, "cache-open", Some("v1")).unwrap();
        assert_ne!(replacement["id"], id);
        drop(b);
        assert!(!shed.lock().unwrap()[&format!("cache-storage-data:{owner}")].contains_key(&id));
        assert_eq!(
            names(&shed.lock().unwrap(), owner).entries[0].1,
            replacement["id"].as_str().unwrap()
        );
    }
    #[test]
    fn cache_names_preserve_order_and_partition_ownership() {
        let shed: crate::js::WebStorage = Arc::default();
        let mut agent = Agent::default();
        for name in ["a", "b", "a"] {
            change(&mut agent, &shed, "origin^site1", "cache-open", Some(name)).unwrap();
        }
        assert_eq!(
            names(&shed.lock().unwrap(), "origin^site1")
                .entries
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(
            change(&mut agent, &shed, "origin^site2", "cache-list", None)
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
