use core::pin::Pin;
use std::{
    cmp::{max, Ordering},
    collections::{hash_map::DefaultHasher, BinaryHeap, HashMap, HashSet},
    hash::{Hash, Hasher},
    net::IpAddr,
    ops::{Add, Deref},
    sync::Arc,
    time::Instant,
};
use tokio::sync::{Mutex, Notify, RwLock};
use trust_dns_resolver::{
    config::LookupIpStrategy, error::ResolveError, lookup_ip::LookupIp, AsyncResolver, TokioAsyncResolver,
};

/// The minimum time that is waited before refreshing the dns cache even though there are entries with a TTL of 0.
const MIN_TIME_BEFORE_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);

/// Represents an entry in the DNS refresh queue. Entries define a custom ordering based on the TTLs of their corresponding DNS cache entries.
#[derive(Debug, Clone)]
struct DnsRefreshQueueEntry {
    /// A copy of the DNS cache entry that should be refreshed (with shared references to the lookup result and watchers).
    cache_entry: DnsCacheEntry,
}

impl Eq for DnsRefreshQueueEntry {}

impl PartialEq for DnsRefreshQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cache_entry
            .lookup_result
            .valid_until()
            .eq(&other.cache_entry.lookup_result.valid_until())
    }
}

impl Ord for DnsRefreshQueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cache_entry
            .lookup_result
            .valid_until()
            .cmp(&other.cache_entry.lookup_result.valid_until())
            .reverse()
    }
}

impl PartialOrd for DnsRefreshQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Represents an entry in the DNS cache.
#[derive(Debug, Clone)]
struct DnsCacheEntry {
    /// The (host-)name for which this entry is a cached result.
    name: String,
    /// A reference to the cached lookup result.
    lookup_result: Arc<LookupIp>,
    /// A shared mutable reference to a set of watcher senders for the watchers that want to be notified of changes to this entry.
    watchers: Arc<RwLock<HashSet<Arc<Pin<Box<DnsWatcherSender>>>>>>,
}

/// DNS resolution cache for the DNS service.
#[derive(Debug, Clone)]
struct DnsServiceCache {
    /// Resolver instance used to resolve DNS entries.
    resolver: TokioAsyncResolver,
    /// Refresh queue to refresh expiring DNS entries (using the DnsServices auto_refresher_task()).
    refresh_queue: BinaryHeap<DnsRefreshQueueEntry>,
    /// Cache entries for the DNS cache.
    cache_data: HashMap<String, DnsCacheEntry>,
}

impl DnsServiceCache {
    fn new() -> Result<DnsServiceCache, ResolveError> {
        let (resolver_conf, mut resolver_opts) = trust_dns_resolver::system_conf::read_system_conf()?;
        resolver_opts.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
        Ok(DnsServiceCache {
            resolver: AsyncResolver::tokio(resolver_conf, resolver_opts)?,
            refresh_queue: Default::default(),
            cache_data: Default::default(),
        })
    }

    /// Returns a DNS resolution result if it is cached, None otherwise.
    fn resolve_if_cached(&self, name: &str) -> Option<DnsCacheEntry> {
        self.cache_data.get(name).map(|v| v.deref().clone())
    }

    /// Resolves a supplied name and adds it to the DNS cache.
    async fn lookup_and_cache(&mut self, name: &str) -> Result<DnsCacheEntry, ResolveError> {
        let lookup_result = DnsCacheEntry {
            name: String::from(name),
            lookup_result: Arc::new(self.resolver.lookup_ip(name).await?),
            watchers: Arc::new(RwLock::new(HashSet::new())),
        };
        self.cache_data.insert(name.into(), lookup_result);
        self.refresh_queue.push(DnsRefreshQueueEntry {
            cache_entry: self.cache_data.get(name.into()).unwrap().clone(),
        });
        Ok(self.cache_data.get(name.into()).unwrap().clone())
    }

    /// Resolves the supplied DNS name. If the name is already in the DNS cache, returns the cached result instead.
    async fn resolve(&mut self, name: &str) -> Result<DnsCacheEntry, ResolveError> {
        match self.cache_data.get(name) {
            Some(v) => Ok(v.clone()),
            None => self.lookup_and_cache(name).await,
        }
    }
}

/// Helper struct used to notify DNS watchers of changes to watched cache entries.
#[derive(Debug)]
struct DnsWatcherSender {
    updated_names: Arc<Mutex<HashSet<String>>>,
    notify: Arc<Notify>,
}

impl Eq for DnsWatcherSender {}

impl PartialEq for DnsWatcherSender {
    fn eq(&self, other: &Self) -> bool {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        let v1 = hasher.finish();
        let mut hasher = DefaultHasher::new();
        other.hash(&mut hasher);
        v1.eq(&hasher.finish())
    }
}

impl Hash for DnsWatcherSender {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self as *const DnsWatcherSender).hash(state);
    }
}

/// DNS Service which provides methods to query a DNS cache for entries and
pub(crate) struct DnsService {
    cache: Arc<RwLock<DnsServiceCache>>,
}

impl DnsService {
    pub fn new() -> Result<DnsService, ResolveError> {
        Ok(DnsService {
            cache: Arc::new(RwLock::new(DnsServiceCache::new()?)),
        })
    }

    /// Asynchronous task to automatically refresh dns cache entries as they expire.
    pub async fn auto_refresher_task(&mut self) {
        let mut next_expiry_time = None;
        loop {
            tokio::time::sleep_until(max(
                next_expiry_time.unwrap_or(Instant::now()).into(),
                Instant::now().add(MIN_TIME_BEFORE_REFRESH).into(),
            ))
            .await;
            debug!("Starting new update cycle of DNS cache.");
            let mut cache = self.cache.write().await;
            let refresh_start = Instant::now();
            let mut watchers_to_notify = HashSet::new();
            let mut new_entries = Vec::new();
            while let Some(queue_element) = cache.refresh_queue.pop() {
                if let Some(duration_until_invalid) = queue_element
                    .cache_entry
                    .lookup_result
                    .valid_until()
                    .checked_duration_since(refresh_start)
                {
                    if duration_until_invalid > MIN_TIME_BEFORE_REFRESH {
                        // Return last queue element to queue.
                        cache.refresh_queue.push(queue_element);
                        break;
                    }
                }
                let name = queue_element.cache_entry.name.clone();
                debug!("Refreshing DNS cache entry for {:?} because cache entry expired.", name);
                let new_entry = cache.resolver.lookup_ip(name.as_str()).await.map(|v| DnsCacheEntry {
                    name: name.clone(),
                    lookup_result: Arc::new(v),
                    watchers: queue_element.cache_entry.watchers.clone(),
                });
                if let Ok(new_entry) = new_entry {
                    let new_set: HashSet<IpAddr> = new_entry.lookup_result.iter().collect();
                    let old_set: HashSet<IpAddr> = queue_element.cache_entry.lookup_result.iter().collect();
                    if !new_set.eq(&old_set) {
                        debug!(
                            "IP address set for {:?} has changed from {:?} to {:?}, notifying watchers of DNS entry change.",
                            name, old_set, new_set
                        );
                        let watchers = new_entry.watchers.read().await;
                        for w in watchers.iter() {
                            watchers_to_notify.insert(w.clone());
                            w.updated_names.lock().await.insert(name.clone());
                        }
                    }
                    cache.cache_data.remove(name.as_str());
                    cache.cache_data.insert(name.clone(), new_entry);
                    new_entries.push(DnsRefreshQueueEntry {
                        cache_entry: cache.cache_data.get(name.as_str()).unwrap().clone(),
                    });
                } else {
                    new_entries.push(queue_element);
                }
            }
            cache.refresh_queue.append(&mut new_entries.into());
            watchers_to_notify.iter().for_each(|w| w.notify.notify_one());
            next_expiry_time = cache
                .refresh_queue
                .peek()
                .map(|v| v.cache_entry.lookup_result.valid_until());
            debug!("Finished DNS cache refresh cycle.");
        }
    }

    /// Create a DnsWatcher instance which can be used to keep track of dns entry changes.
    pub fn create_watcher(&self) -> DnsWatcher {
        DnsWatcher {
            cache: self.cache.clone(),
            sender: Arc::new(Pin::new(Box::new(DnsWatcherSender {
                updated_names: Default::default(),
                notify: Default::default(),
            }))),
            current_watched_entries: Default::default(),
        }
    }
}

/// Struct which can be used to keep track of dns entry changes.
pub(crate) struct DnsWatcher {
    /// Reference to the DNS cache of the DnsService.
    cache: Arc<RwLock<DnsServiceCache>>,
    /// Reference to a sender instance which is added to cache entries if the watches wishes to be notified of changes.
    sender: Arc<Pin<Box<DnsWatcherSender>>>,
    /// Set of currently watched DNS entries.
    current_watched_entries: Mutex<HashSet<String>>,
}

impl DnsWatcher {
    /// Resolves the given DNS name and adds the name to the list of watched DNS entries.
    pub async fn resolve_and_watch(&self, name: &str) -> Result<LookupIp, ResolveError> {
        let mut cache = self.cache.write().await;
        let resolved_value = cache.resolve(name).await?;
        resolved_value.watchers.write().await.insert(self.sender.clone());
        self.current_watched_entries.lock().await.insert(name.into());
        Ok(resolved_value.lookup_result.deref().clone())
    }

    /// Removes a name from the list of watched DNS entries.
    pub async fn remove_watched_name(&self, name: &str) {
        let cache = self.cache.read().await;
        let cache_entry = cache.resolve_if_cached(name).unwrap();
        cache_entry.watchers.write().await.remove(&self.sender.clone());
        self.current_watched_entries.lock().await.remove(name.into());
    }

    /// Clears the list of watched DNS entries.
    pub async fn clear_watched_names(&self) {
        for name in self.current_watched_entries.lock().await.clone() {
            self.remove_watched_name(name.as_str()).await;
        }
    }

    /// Yield until a change to any of the watched DNS entries of this watcher occurs.
    /// Returns immediately in case a change has already happened but was not waited for.
    pub async fn address_changed(&self) -> () {
        self.sender.notify.notified().await
    }
}
