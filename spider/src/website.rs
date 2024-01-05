use crate::black_list::contains;
use crate::configuration::{get_ua, Configuration, RedirectPolicy};
use crate::packages::robotparser::parser::RobotFileParser;
use crate::page::{build, get_page_selectors, Page};
use crate::utils::log;
use crate::CaseInsensitiveString;
use crate::Client;

use compact_str::CompactString;
use hashbrown::{HashMap, HashSet};
use reqwest::redirect::Policy;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, Semaphore};
use tokio::task;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use url::Url;

#[cfg(feature = "chrome")]
use crate::features::chrome::{configure_browser, launch_browser};

#[cfg(feature = "cron")]
use async_job::{async_trait, Job, Runner};
#[cfg(feature = "napi")]
use napi::bindgen_prelude::*;

#[cfg(not(feature = "decentralized"))]
lazy_static! {
    static ref SEM: Semaphore = {
        let logical = num_cpus::get();
        let physical = num_cpus::get_physical();

        let sem_limit = if logical > physical {
            (logical) / (physical) as usize
        } else {
            logical
        };

        let (sem_limit, sem_max) = if logical == physical {
            (sem_limit * physical, 50)
        } else {
            (sem_limit * 4, 25)
        };
        let sem_limit = if cfg!(feature = "chrome") {
            sem_limit / 2
        } else {
            sem_limit
        };
        Semaphore::const_new(sem_limit.max(sem_max))
    };
}

#[cfg(feature = "decentralized")]
lazy_static! {
    static ref WORKERS: HashSet<String> = {
        let mut set: HashSet<_> = HashSet::new();

        for worker in std::env::var("SPIDER_WORKER_SCRAPER")
            .unwrap_or_else(|_| "http://127.0.0.1:3031".to_string())
            .split(",")
        {
            set.insert(worker.to_string());
        }

        for worker in std::env::var("SPIDER_WORKER")
            .unwrap_or_else(|_| "http://127.0.0.1:3030".to_string())
            .split(",")
        {
            set.insert(worker.to_string());
        }

        set
    };
    static ref SEM: Semaphore = {
        let logical = num_cpus::get();
        let physical = num_cpus::get_physical();

        let sem_limit = if logical > physical {
            (logical) / (physical) as usize
        } else {
            logical
        };

        let (sem_limit, sem_max) = if logical == physical {
            (sem_limit * physical, 75)
        } else {
            (sem_limit * 4, 33)
        };
        let (sem_limit, sem_max) = { (sem_limit * WORKERS.len(), sem_max * WORKERS.len()) };

        Semaphore::const_new(sem_limit.max(sem_max))
    };
}

#[cfg(feature = "budget")]
lazy_static! {
    static ref WILD_CARD_PATH: CaseInsensitiveString = CaseInsensitiveString::from("*");
}

/// the active status of the crawl.
#[derive(Debug, Clone, Default, PartialEq, Eq, strum::EnumString, strum::Display)]
pub enum CrawlStatus {
    /// The crawl did not start yet.
    #[default]
    Start,
    /// The crawl is idle and has completed.
    Idle,
    /// The crawl is active.
    Active,
    /// The crawl blocked from network ratelimit, firewall, etc.
    Blocked,
    /// The initial request ran without returning html.
    Empty,
    /// The URL of the website is invalid. Crawl cannot commence.
    Invalid,
    #[cfg(feature = "control")]
    /// The crawl shutdown manually.
    Shutdown,
    #[cfg(feature = "control")]
    /// The crawl paused manually.
    Paused,
}

/// The type of cron job to run
#[derive(Debug, Clone, Default, PartialEq, Eq, strum::EnumString, strum::Display)]
pub enum CronType {
    #[default]
    /// Crawl collecting links, page data, and etc.
    Crawl,
    /// Scrape collecting links, page data as bytes to store, and etc.
    Scrape,
}

/// Represents a website to crawl and gather all links or page content.
/// ```rust
/// use spider::website::Website;
/// let mut website = Website::new("http://example.com");
/// website.crawl();
/// // `Website` will be filled with links or pages when crawled. If you need pages with the resource
/// // call the `website.scrape` method with `website.get_pages` instead.
/// for link in website.get_links() {
///     // do something
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct Website {
    /// Configuration properties for website.
    pub configuration: Box<Configuration>,
    /// All URLs visited.
    links_visited: Box<HashSet<CaseInsensitiveString>>,
    /// Pages visited.
    pages: Option<Box<Vec<Page>>>,
    /// Robot.txt parser.
    robot_file_parser: Option<Box<RobotFileParser>>,
    /// Base root domain of the crawl.
    domain: Box<CaseInsensitiveString>,
    /// The domain url parsed.
    domain_parsed: Option<Box<Url>>,
    /// The callback when a link is found.
    pub on_link_find_callback: Option<
        fn(CaseInsensitiveString, Option<String>) -> (CaseInsensitiveString, Option<String>),
    >,
    /// Subscribe and broadcast changes.
    channel: Option<(broadcast::Sender<Page>, Arc<broadcast::Receiver<Page>>)>,
    /// The status of the active crawl.
    status: CrawlStatus,
    /// Set the crawl ID to track. This allows explicit targeting for shutdown, pause, and etc.
    #[cfg(feature = "control")]
    pub crawl_id: Box<String>,
    /// The website was manually stopped.
    shutdown: bool,
    /// The request client. Stored for re-use between runs.
    client: Option<Client>,
}

impl Website {
    /// Initialize Website object with a start link to crawl.
    pub fn new(url: &str) -> Self {
        let domain: Box<CaseInsensitiveString> = if url.starts_with("http") {
            CaseInsensitiveString::new(&url).into()
        } else {
            CaseInsensitiveString::new(&string_concat!("https://", url)).into()
        };
        Self {
            configuration: Configuration::new().into(),
            links_visited: Box::new(HashSet::new()),
            pages: None,
            robot_file_parser: None,
            on_link_find_callback: None,
            channel: None,
            status: CrawlStatus::Start,
            shutdown: false,
            domain_parsed: match url::Url::parse(&domain.inner()) {
                Ok(u) => Some(Box::new(crate::page::convert_abs_path(&u, "/"))),
                _ => None,
            },
            domain,
            ..Default::default()
        }
    }

    /// return `true` if URL:
    ///
    /// - is not already crawled
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    #[cfg(all(not(feature = "regex"), not(feature = "budget")))]
    pub fn is_allowed(
        &self,
        link: &CaseInsensitiveString,
        blacklist_url: &Box<Vec<CompactString>>,
    ) -> bool {
        if self.links_visited.contains(link) {
            false
        } else {
            self.is_allowed_default(&link.inner(), blacklist_url)
        }
    }

    /// return `true` if URL:
    ///
    /// - is not already crawled
    /// - is not over crawl budget
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    #[cfg(all(not(feature = "regex"), feature = "budget"))]
    pub fn is_allowed(
        &mut self,
        link: &CaseInsensitiveString,
        blacklist_url: &Box<Vec<CompactString>>,
    ) -> bool {
        if self.links_visited.contains(link) {
            false
        } else if self.is_over_budget(&link) {
            false
        } else {
            self.is_allowed_default(&link.inner(), blacklist_url)
        }
    }

    /// return `true` if URL:
    ///
    /// - is not already crawled
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    #[cfg(all(feature = "regex", not(feature = "budget")))]
    pub fn is_allowed(
        &self,
        link: &CaseInsensitiveString,
        blacklist_url: &Box<regex::RegexSet>,
    ) -> bool {
        if self.links_visited.contains(link) {
            false
        } else {
            self.is_allowed_default(link, blacklist_url)
        }
    }

    /// return `true` if URL:
    ///
    /// - is not already crawled
    /// - is not over crawl budget
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    #[cfg(all(feature = "regex", feature = "budget"))]
    pub fn is_allowed(
        &mut self,
        link: &CaseInsensitiveString,
        blacklist_url: &Box<regex::RegexSet>,
    ) -> bool {
        if self.links_visited.contains(link) {
            false
        } else if self.is_over_budget(&link) {
            false
        } else {
            self.is_allowed_default(link, blacklist_url)
        }
    }

    /// return `true` if URL:
    ///
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    #[cfg(feature = "regex")]
    pub fn is_allowed_default(
        &self,
        link: &CaseInsensitiveString,
        blacklist_url: &Box<regex::RegexSet>,
    ) -> bool {
        if !blacklist_url.is_empty() {
            !contains(blacklist_url, &link.inner())
        } else {
            self.is_allowed_robots(&link.as_ref())
        }
    }

    /// return `true` if URL:
    ///
    /// - is not blacklisted
    /// - is not forbidden in robot.txt file (if parameter is defined)
    #[inline]
    #[cfg(not(feature = "regex"))]
    pub fn is_allowed_default(
        &self,
        link: &CompactString,
        blacklist_url: &Box<Vec<CompactString>>,
    ) -> bool {
        if contains(blacklist_url, &link) {
            false
        } else {
            self.is_allowed_robots(&link)
        }
    }

    /// return `true` if URL:
    ///
    /// - is not forbidden in robot.txt file (if parameter is defined)
    pub fn is_allowed_robots(&self, link: &str) -> bool {
        if self.configuration.respect_robots_txt {
            unsafe {
                self.robot_file_parser
                    .as_ref()
                    .unwrap_unchecked()
                    .can_fetch("*", &link)
            } // unwrap will always return
        } else {
            true
        }
    }

    #[cfg(feature = "budget")]
    /// Validate if url exceeds crawl budget and should not be handled.
    pub fn is_over_budget(&mut self, link: &CaseInsensitiveString) -> bool {
        if self.configuration.budget.is_some() || self.configuration.depth_distance > 0 {
            match Url::parse(&link.inner()) {
                Ok(r) => {
                    let has_depth_control = self.configuration.depth_distance > 0;

                    if !self.configuration.budget.is_some() {
                        match r.path_segments() {
                            Some(mut segments) => {
                                let mut over = false;
                                let mut depth: usize = 0;

                                while let Some(_) = segments.next() {
                                    if has_depth_control {
                                        if depth != usize::MAX {
                                            depth += 1;
                                        }
                                        if depth >= self.configuration.depth_distance {
                                            over = true;
                                            break;
                                        }
                                    }
                                }

                                over
                            }
                            _ => false,
                        }
                    } else {
                        match self.configuration.budget.as_mut() {
                            Some(budget) => {
                                let exceeded_wild_budget = if self.configuration.wild_card_budgeting
                                {
                                    match budget.get_mut(&*WILD_CARD_PATH) {
                                        Some(budget) => {
                                            if budget.abs_diff(0) == 1 {
                                                true
                                            } else {
                                                *budget -= 1;
                                                false
                                            }
                                        }
                                        _ => false,
                                    }
                                } else {
                                    false
                                };

                                // set this up prior to crawl to avoid checks per link
                                let skip_paths =
                                    self.configuration.wild_card_budgeting && budget.len() == 1;

                                // check if paths pass
                                if !skip_paths && !exceeded_wild_budget {
                                    match r.path_segments() {
                                        Some(mut segments) => {
                                            let mut joint_segment =
                                                CaseInsensitiveString::default();
                                            let mut over = false;
                                            let mut depth: usize = 0;

                                            while let Some(seg) = segments.next() {
                                                if has_depth_control {
                                                    if depth != usize::MAX {
                                                        depth += 1;
                                                    }
                                                    if depth >= self.configuration.depth_distance {
                                                        over = true;
                                                        break;
                                                    }
                                                }

                                                joint_segment.push_str(seg);

                                                if budget.contains_key(&joint_segment) {
                                                    match budget.get_mut(&joint_segment) {
                                                        Some(budget) => {
                                                            if budget.abs_diff(0) == 0
                                                                || *budget == 0
                                                            {
                                                                over = true;
                                                                break;
                                                            } else {
                                                                *budget -= 1;
                                                                continue;
                                                            }
                                                        }
                                                        _ => (),
                                                    };
                                                }
                                            }

                                            over
                                        }
                                        _ => false,
                                    }
                                } else {
                                    exceeded_wild_budget
                                }
                            }
                            _ => false,
                        }
                    }
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// Amount of pages crawled.
    pub fn size(&self) -> usize {
        self.links_visited.len()
    }

    /// Page getter.
    pub fn get_pages(&self) -> Option<&Box<Vec<Page>>> {
        self.pages.as_ref()
    }

    /// Drain the links visited.
    pub fn drain_links(&mut self) -> hashbrown::hash_set::Drain<'_, CaseInsensitiveString> {
        self.links_visited.drain()
    }

    /// Clear all pages and links stored.
    pub fn clear(&mut self) {
        self.links_visited.clear();
        self.pages.take();
    }

    /// Links visited getter.
    pub fn get_links(&self) -> &HashSet<CaseInsensitiveString> {
        &self.links_visited
    }

    /// Domain parsed url getter.
    pub fn get_domain_parsed(&self) -> &Option<Box<Url>> {
        &self.domain_parsed
    }

    /// Domain name getter.
    pub fn get_domain(&self) -> &CaseInsensitiveString {
        &self.domain
    }

    /// Crawl delay getter.
    fn get_delay(&self) -> Duration {
        Duration::from_millis(self.configuration.delay)
    }

    /// Get the active crawl status.
    pub fn get_status(&self) -> &CrawlStatus {
        &self.status
    }

    /// Set the crawl status to persist between the run.
    /// Example crawling a sitemap and all links after - website.crawl_sitemap().await.persist_links().crawl().await
    pub fn persist_links(&mut self) -> &mut Self {
        self.status = CrawlStatus::Active;
        self
    }

    /// Absolute base url of crawl.
    pub fn get_absolute_path(&self, domain: Option<&str>) -> Option<Url> {
        if domain.is_some() {
            match url::Url::parse(domain.unwrap_or_default()) {
                Ok(u) => Some(crate::page::convert_abs_path(&u, "/")),
                _ => None,
            }
        } else {
            self.domain_parsed.as_deref().cloned()
        }
    }

    /// Stop all crawls for the website.
    pub fn stop(&mut self) {
        self.shutdown = true;
    }

    /// Crawls commenced from fresh run.
    fn start(&mut self) {
        self.shutdown = false;
    }

    /// configure the robots parser on initial crawl attempt and run.
    pub async fn configure_robots_parser(&mut self, client: Client) -> Client {
        if self.configuration.respect_robots_txt {
            let robot_file_parser = self
                .robot_file_parser
                .get_or_insert_with(|| RobotFileParser::new());

            if robot_file_parser.mtime() <= 4000 {
                let host_str = match &self.domain_parsed {
                    Some(domain) => &*domain.as_str(),
                    _ => &self.domain.inner(),
                };
                if host_str.ends_with("/") {
                    robot_file_parser.read(&client, &host_str).await;
                } else {
                    robot_file_parser
                        .read(&client, &string_concat!(host_str, "/"))
                        .await;
                }
                self.configuration.delay = robot_file_parser
                    .get_crawl_delay(&self.configuration.user_agent) // returns the crawl delay in seconds
                    .unwrap_or_else(|| self.get_delay())
                    .as_millis() as u64;
            }
        }

        client
    }

    /// Setup strict a strict redirect policy for request.
    fn setup_strict_policy(&self) -> Policy {
        use crate::page::domain_name;
        use reqwest::redirect::Attempt;
        use std::sync::atomic::AtomicU8;

        let host_str = self.domain_parsed.as_deref().cloned();
        let default_policy = reqwest::redirect::Policy::default();

        match host_str {
            Some(host_s) => {
                let initial_redirect = Arc::new(AtomicU8::new(0));
                let initial_redirect_limit = if self.configuration.respect_robots_txt {
                    2
                } else {
                    1
                };
                let subdomains = self.configuration.subdomains;
                let tld = self.configuration.tld;
                let host_domain_name = if tld {
                    domain_name(&host_s).to_string()
                } else {
                    Default::default()
                };
                let redirect_limit = *self.configuration.redirect_limit;

                let custom_policy = {
                    move |attempt: Attempt| {
                        if tld && domain_name(attempt.url()) == host_domain_name
                            || subdomains
                                && attempt
                                    .url()
                                    .host_str()
                                    .unwrap_or_default()
                                    .ends_with(host_s.host_str().unwrap_or_default())
                            || attempt.url().host() == host_s.host()
                        {
                            default_policy.redirect(attempt)
                        } else if attempt.previous().len() > redirect_limit {
                            attempt.error("too many redirects")
                        } else if attempt.status().is_redirection()
                            && (0..initial_redirect_limit)
                                .contains(&initial_redirect.load(Ordering::Relaxed))
                        {
                            initial_redirect.fetch_add(1, Ordering::Relaxed);
                            default_policy.redirect(attempt)
                        } else {
                            attempt.stop()
                        }
                    }
                };
                reqwest::redirect::Policy::custom(custom_policy)
            }
            _ => default_policy,
        }
    }

    /// Setup redirect policy for reqwest.
    fn setup_redirect_policy(&self) -> Policy {
        match self.configuration.redirect_policy {
            RedirectPolicy::Loose => {
                reqwest::redirect::Policy::limited(*self.configuration.redirect_limit)
            }
            RedirectPolicy::Strict => self.setup_strict_policy(),
        }
    }

    /// Build the HTTP client.
    #[cfg(all(not(feature = "decentralized"), not(feature = "cache")))]
    fn configure_http_client_builder(&mut self) -> crate::ClientBuilder {
        let policy = self.setup_redirect_policy();

        let client = Client::builder()
            .user_agent(match &self.configuration.user_agent {
                Some(ua) => ua.as_str(),
                _ => &get_ua(),
            })
            .redirect(policy)
            .tcp_keepalive(Duration::from_millis(500))
            .pool_idle_timeout(None);

        let client = if self.configuration.http2_prior_knowledge {
            client.http2_prior_knowledge()
        } else {
            client
        };

        let client = match &self.configuration.headers {
            Some(headers) => client.default_headers(*headers.to_owned()),
            _ => client,
        };

        let mut client = match &self.configuration.request_timeout {
            Some(t) => client.timeout(**t),
            _ => client,
        };

        let client = match &self.configuration.proxies {
            Some(proxies) => {
                for proxie in proxies.iter() {
                    match reqwest::Proxy::all(proxie) {
                        Ok(proxy) => client = client.proxy(proxy),
                        _ => (),
                    }
                }
                client
            }
            _ => client,
        };

        let client = self.configure_http_client_cookies(client);

        client
    }

    /// Build the HTTP client with caching enabled.
    #[cfg(all(not(feature = "decentralized"), feature = "cache"))]
    fn configure_http_client_builder(&mut self) -> crate::ClientBuilder {
        use crate::page::domain_name;
        use http_cache_reqwest::{CACacheManager, Cache, CacheMode, HttpCache, HttpCacheOptions};
        use reqwest::redirect::Attempt;
        use reqwest_middleware::ClientBuilder;
        use std::sync::atomic::AtomicU8;

        let host_str = self.domain_parsed.as_deref().cloned();
        let default_policy = reqwest::redirect::Policy::default();

        let policy = self.setup_redirect_policy();

        let client = reqwest::Client::builder()
            .user_agent(match &self.configuration.user_agent {
                Some(ua) => ua.as_str(),
                _ => &get_ua(),
            })
            .redirect(policy)
            .tcp_keepalive(Duration::from_millis(500))
            .pool_idle_timeout(None);

        let client = if self.configuration.http2_prior_knowledge {
            client.http2_prior_knowledge()
        } else {
            client
        };

        let client = match &self.configuration.headers {
            Some(headers) => client.default_headers(*headers.to_owned()),
            _ => client,
        };

        let mut client = match &self.configuration.request_timeout {
            Some(t) => client.timeout(**t),
            _ => client,
        };

        let client = match &self.configuration.proxies {
            Some(proxies) => {
                for proxie in proxies.iter() {
                    match reqwest::Proxy::all(proxie) {
                        Ok(proxy) => client = client.proxy(proxy),
                        _ => (),
                    }
                }
                client
            }
            _ => client,
        };

        let client = self.configure_http_client_cookies(client);
        let client = ClientBuilder::new(unsafe { client.build().unwrap_unchecked() });

        if self.configuration.cache {
            client.with(Cache(HttpCache {
                mode: CacheMode::Default,
                manager: CACacheManager::default(),
                options: HttpCacheOptions::default(),
            }))
        } else {
            client
        }
    }

    /// Build the HTTP client with cookie configurations.
    #[cfg(all(not(feature = "decentralized"), feature = "cookies"))]
    fn configure_http_client_cookies(
        &mut self,
        client: reqwest::ClientBuilder,
    ) -> reqwest::ClientBuilder {
        let client = client.cookie_store(true);
        let client = if !self.configuration.cookie_str.is_empty() && self.domain_parsed.is_some() {
            match self.domain_parsed.clone() {
                Some(p) => {
                    let cookie_store = reqwest::cookie::Jar::default();
                    cookie_store.add_cookie_str(&self.configuration.cookie_str, &p);
                    client.cookie_provider(cookie_store.into())
                }
                _ => client,
            }
        } else {
            client
        };
        client
    }

    /// Build the client with cookie configurations. This does nothing with [cookies] flag enabled.
    #[cfg(all(not(feature = "decentralized"), not(feature = "cookies")))]
    fn configure_http_client_cookies(
        &mut self,
        client: reqwest::ClientBuilder,
    ) -> reqwest::ClientBuilder {
        client
    }

    /// Configure http client.
    #[cfg(all(not(feature = "decentralized"), not(feature = "cache")))]
    pub fn configure_http_client(&mut self) -> Client {
        let client = self.configure_http_client_builder();
        // should unwrap using native-tls-alpn
        unsafe { client.build().unwrap_unchecked() }
    }

    /// Configure http client.
    #[cfg(all(not(feature = "decentralized"), feature = "cache"))]
    pub fn configure_http_client(&mut self) -> Client {
        let client = self.configure_http_client_builder();
        client.build()
    }

    /// Configure http client for decentralization.
    #[cfg(all(feature = "decentralized", not(feature = "cache")))]
    pub fn configure_http_client(&mut self) -> Client {
        use reqwest::header::HeaderMap;
        use reqwest::header::HeaderValue;

        let mut headers = HeaderMap::new();

        let policy = self.setup_redirect_policy();

        let mut client = Client::builder()
            .user_agent(match &self.configuration.user_agent {
                Some(ua) => ua.as_str(),
                _ => &get_ua(),
            })
            .redirect(policy)
            .tcp_keepalive(Duration::from_millis(500))
            .pool_idle_timeout(None);

        let referer = if self.configuration.tld && self.configuration.subdomains {
            2
        } else if self.configuration.tld {
            2
        } else if self.configuration.subdomains {
            1
        } else {
            0
        };

        if referer > 0 {
            // use expected http headers for providers that drop invalid headers
            headers.insert(reqwest::header::REFERER, HeaderValue::from(referer));
        }

        match &self.configuration.headers {
            Some(h) => headers.extend(*h.to_owned()),
            _ => (),
        };

        match self.get_absolute_path(None) {
            Some(domain_url) => {
                let domain_url = domain_url.as_str();
                let domain_host = if domain_url.ends_with("/") {
                    &domain_url[0..domain_url.len() - 1]
                } else {
                    domain_url
                };
                match HeaderValue::from_str(domain_host) {
                    Ok(value) => {
                        headers.insert(reqwest::header::HOST, value);
                    }
                    _ => (),
                }
            }
            _ => (),
        }

        for worker in WORKERS.iter() {
            match reqwest::Proxy::all(worker) {
                Ok(worker) => {
                    client = client.proxy(worker);
                }
                _ => (),
            }
        }

        // should unwrap using native-tls-alpn
        unsafe {
            match &self.configuration.request_timeout {
                Some(t) => client.timeout(**t),
                _ => client,
            }
            .default_headers(headers)
            .build()
            .unwrap_unchecked()
        }
    }

    /// Configure http client for decentralization.
    #[cfg(all(feature = "decentralized", feature = "cache"))]
    pub fn configure_http_client(&mut self) -> Client {
        use http_cache_reqwest::{CACacheManager, Cache, CacheMode, HttpCache, HttpCacheOptions};
        use reqwest::header::HeaderMap;
        use reqwest::header::HeaderValue;
        use reqwest_middleware::ClientBuilder;

        let mut headers = HeaderMap::new();

        let policy = self.setup_redirect_policy();

        let mut client = reqwest::Client::builder()
            .user_agent(match &self.configuration.user_agent {
                Some(ua) => ua.as_str(),
                _ => &get_ua(),
            })
            .redirect(policy)
            .tcp_keepalive(Duration::from_millis(500))
            .pool_idle_timeout(None);

        let referer = if self.configuration.tld && self.configuration.subdomains {
            2
        } else if self.configuration.tld {
            2
        } else if self.configuration.subdomains {
            1
        } else {
            0
        };

        if referer > 0 {
            // use expected http headers for providers that drop invalid headers
            headers.insert(reqwest::header::REFERER, HeaderValue::from(referer));
        }

        match &self.configuration.headers {
            Some(h) => headers.extend(*h.to_owned()),
            _ => (),
        };

        match self.get_absolute_path(None) {
            Some(domain_url) => {
                let domain_url = domain_url.as_str();
                let domain_host = if domain_url.ends_with("/") {
                    &domain_url[0..domain_url.len() - 1]
                } else {
                    domain_url
                };
                match HeaderValue::from_str(domain_host) {
                    Ok(value) => {
                        headers.insert(reqwest::header::HOST, value);
                    }
                    _ => (),
                }
            }
            _ => (),
        }

        for worker in WORKERS.iter() {
            match reqwest::Proxy::all(worker) {
                Ok(worker) => {
                    client = client.proxy(worker);
                }
                _ => (),
            }
        }

        let client = ClientBuilder::new(unsafe {
            match &self.configuration.request_timeout {
                Some(t) => client.timeout(**t),
                _ => client,
            }
            .default_headers(headers)
            .build()
            .unwrap_unchecked()
        })
        .with(Cache(HttpCache {
            mode: CacheMode::Default,
            manager: CACacheManager::default(),
            options: HttpCacheOptions::default(),
        }));

        client.build()
    }

    /// Setup atomic controller.
    #[cfg(feature = "control")]
    fn configure_handler(&self) -> (Arc<AtomicI8>, tokio::task::JoinHandle<()>) {
        use crate::utils::{Handler, CONTROLLER};
        let c: Arc<AtomicI8> = Arc::new(AtomicI8::new(0));
        let handle = c.clone();
        let target_id = string_concat!(self.crawl_id, self.domain.inner());
        let c_lock = CONTROLLER.clone();

        let join_handle = tokio::spawn(async move {
            let mut l = c_lock.read().await.1.to_owned();

            while l.changed().await.is_ok() {
                let n = &*l.borrow();
                let (target, rest) = n;

                if target_id.eq_ignore_ascii_case(&target) {
                    if rest == &Handler::Resume {
                        c.store(0, Ordering::Relaxed);
                    }
                    if rest == &Handler::Pause {
                        c.store(1, Ordering::Relaxed);
                    }
                    if rest == &Handler::Shutdown {
                        c.store(2, Ordering::Relaxed);
                    }
                }
            }
        });

        (handle, join_handle)
    }

    /// Setup interception for chrome request.
    #[cfg(all(feature = "chrome", feature = "chrome_intercept"))]
    async fn setup_chrome_interception(
        &self,
        chrome_page: &Arc<chromiumoxide::Page>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if self.configuration.chrome_intercept {
            use chromiumoxide::cdp::browser_protocol::network::ResourceType;

            match chrome_page
                .event_listener::<chromiumoxide::cdp::browser_protocol::fetch::EventRequestPaused>()
                .await
            {
                Ok(mut rp) => {
                    let mut host_name = self.domain.inner().to_string();
                    let intercept_page = chrome_page.clone();
                    let ignore_visuals = self.configuration.chrome_intercept_block_visuals;

                    let ih = task::spawn(async move {
                        let mut first_rq = true;
                        while let Some(event) = rp.next().await {
                            let u = &event.request.url;

                            if first_rq {
                                if ResourceType::Document == event.resource_type {
                                    host_name = u.into();
                                }
                                first_rq = false;
                            }

                            if 
                                ignore_visuals && (ResourceType::Image == event.resource_type || ResourceType::Media == event.resource_type || ResourceType::Stylesheet == event.resource_type) ||
                                ResourceType::Prefetch == event.resource_type || 
                                ResourceType::Ping == event.resource_type ||
                                ResourceType::Script == event.resource_type && !(u.starts_with('/') || u.starts_with(&host_name) || crate::page::JS_FRAMEWORK_ALLOW.contains(&u.as_str()) || u.starts_with("https://js.stripe.com/v3/")) // add one off stripe framework check for now...
                            {
                                match chromiumoxide::cdp::browser_protocol::fetch::FulfillRequestParams::builder()
                                .request_id(event.request_id.clone())
                                .response_code(200)
                                .build() {
                                    Ok(c) => {
                                        if let Err(e) = intercept_page.execute(c).await
                                        {
                                            log("Failed to fullfill request: ", e.to_string());
                                        }
                                    }
                                    _ => {
                                        log("Failed to get request handle ", &host_name);
                                    }
                                }
                        } else if let Err(e) = intercept_page
                            .execute(chromiumoxide::cdp::browser_protocol::fetch::ContinueRequestParams::new(event.request_id.clone()))
                            .await
                            {
                                log("Failed to continue request: ", e.to_string());
                            }
                        }
                    });

                    Some(ih)
                }
                _ => None,
            }
        } else {
            None
        }
    }

    /// Setup interception for chrome request
    #[cfg(all(feature = "chrome", not(feature = "chrome_intercept")))]
    async fn setup_chrome_interception(
        &self,
        _chrome_page: &Arc<chromiumoxide::Page>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }

    /// Setup selectors for handling link targets.
    fn setup_selectors(&self) -> Option<(CompactString, smallvec::SmallVec<[CompactString; 2]>)> {
        get_page_selectors(
            &self.domain.inner(),
            self.configuration.subdomains,
            self.configuration.tld,
        )
    }

    /// Setup config for crawl.
    #[cfg(feature = "control")]
    async fn setup(&mut self) -> (Client, Option<(Arc<AtomicI8>, tokio::task::JoinHandle<()>)>) {
        self.determine_limits();
        if self.status == CrawlStatus::Idle {
            self.clear();
        }
        let client = self.configure_http_client();

        (
            self.configure_robots_parser(client).await,
            Some(self.configure_handler()),
        )
    }

    /// Setup config for crawl.
    #[cfg(not(feature = "control"))]
    async fn setup(&mut self) -> (Client, Option<(Arc<AtomicI8>, tokio::task::JoinHandle<()>)>) {
        self.determine_limits();
        if self.status == CrawlStatus::Idle {
            self.clear();
        }

        let client = match self.client.take() {
            Some(client) => client,
            _ => self.configure_http_client(),
        };

        (self.configure_robots_parser(client).await, None)
    }

    /// Setup shared concurrent configs.
    fn setup_crawl(
        &mut self,
    ) -> (
        std::pin::Pin<Box<tokio::time::Interval>>,
        std::pin::Pin<Box<Duration>>,
    ) {
        self.status = CrawlStatus::Active;
        let interval = Box::pin(tokio::time::interval(Duration::from_millis(10)));
        let throttle = Box::pin(self.get_delay());

        (interval, throttle)
    }

    /// Expand links for crawl.
    async fn _crawl_establish(
        &mut self,
        client: &Client,
        base: &mut (CompactString, smallvec::SmallVec<[CompactString; 2]>),
        _: bool,
    ) -> HashSet<CaseInsensitiveString> {
        let links: HashSet<CaseInsensitiveString> = if self
            .is_allowed_default(&self.get_base_link(), &self.configuration.get_blacklist())
        {
            let page = Page::new_page(&self.domain.inner(), &client).await;

            // allow initial page mutation
            match page.final_redirect_destination {
                Some(ref domain) => {
                    let domain: Box<CaseInsensitiveString> =
                        CaseInsensitiveString::new(&domain).into();
                    self.domain_parsed = match url::Url::parse(&domain.inner()) {
                        Ok(u) => Some(Box::new(crate::page::convert_abs_path(&u, "/"))),
                        _ => None,
                    };
                    self.domain = domain;
                    match self.setup_selectors() {
                        Some(s) => {
                            base.0 = s.0;
                            base.1 = s.1;
                        }
                        _ => (),
                    }
                }
                _ => (),
            };

            let links = if !page.is_empty() {
                self.links_visited.insert(match self.on_link_find_callback {
                    Some(cb) => {
                        let c = cb(*self.domain.clone(), None);

                        c.0
                    }
                    _ => *self.domain.clone(),
                });

                let links = HashSet::from(page.links(&base).await);

                links
            } else {
                self.status = CrawlStatus::Empty;
                Default::default()
            };

            channel_send_page(&self.channel, page);

            links
        } else {
            HashSet::new()
        };

        links
    }

    /// Expand links for crawl.
    #[cfg(all(
        not(feature = "glob"),
        not(feature = "decentralized"),
        not(feature = "chrome")
    ))]
    async fn crawl_establish(
        &mut self,
        client: &Client,
        base: &mut (CompactString, smallvec::SmallVec<[CompactString; 2]>),
        selector: bool,
    ) -> HashSet<CaseInsensitiveString> {
        self._crawl_establish(&client, base, selector).await
    }

    /// Expand links for crawl.
    #[cfg(all(
        not(feature = "glob"),
        not(feature = "decentralized"),
        feature = "chrome"
    ))]
    async fn crawl_establish(
        &mut self,
        client: &Client,
        base: &mut (CompactString, smallvec::SmallVec<[CompactString; 2]>),
        _: bool,
        page: &chromiumoxide::Page,
    ) -> HashSet<CaseInsensitiveString> {
        let links: HashSet<CaseInsensitiveString> = if self
            .is_allowed_default(&self.get_base_link(), &self.configuration.get_blacklist())
        {
            let page = Page::new(&self.domain.inner(), &client, &page).await;

            match page.final_redirect_destination {
                Some(ref domain) => {
                    let domain: Box<CaseInsensitiveString> =
                        CaseInsensitiveString::new(&domain).into();
                    self.domain_parsed = match url::Url::parse(&domain.inner()) {
                        Ok(u) => Some(Box::new(crate::page::convert_abs_path(&u, "/"))),
                        _ => None,
                    };
                    self.domain = domain;
                    match self.setup_selectors() {
                        Some(s) => {
                            base.0 = s.0;
                            base.1 = s.1;
                        }
                        _ => (),
                    }
                }
                _ => (),
            }

            let links = if !page.is_empty() {
                self.links_visited.insert(match self.on_link_find_callback {
                    Some(cb) => {
                        let c = cb(*self.domain.clone(), None);

                        c.0
                    }
                    _ => *self.domain.clone(),
                });

                let links = HashSet::from(page.links(&base).await);

                links
            } else {
                self.status = CrawlStatus::Empty;
                Default::default()
            };

            channel_send_page(&self.channel, page);

            links
        } else {
            HashSet::new()
        };

        links
    }

    /// Expand links for crawl.
    #[cfg(all(not(feature = "glob"), feature = "decentralized"))]
    async fn crawl_establish(
        &mut self,
        client: &Client,
        _: &(CompactString, smallvec::SmallVec<[CompactString; 2]>),
        http_worker: bool,
    ) -> HashSet<CaseInsensitiveString> {
        // base_domain name passed here is for primary url determination and not subdomain.tld placement
        let links: HashSet<CaseInsensitiveString> = if self
            .is_allowed_default(&self.get_base_link(), &self.configuration.get_blacklist())
        {
            let link = self.domain.inner();

            let page = Page::new(
                &if http_worker && link.starts_with("https") {
                    link.replacen("https", "http", 1)
                } else {
                    link.to_string()
                },
                &client,
            )
            .await;

            self.links_visited.insert(match self.on_link_find_callback {
                Some(cb) => {
                    let c = cb(*self.domain.to_owned(), None);

                    c.0
                }
                _ => *self.domain.to_owned(),
            });

            let page_links = HashSet::from(page.links.clone());

            channel_send_page(&self.channel, page);

            page_links
        } else {
            HashSet::new()
        };

        links
    }

    /// Expand links for crawl.
    #[cfg(all(feature = "glob", feature = "decentralized"))]
    async fn crawl_establish(
        &mut self,
        client: &Client,
        _: &(CompactString, smallvec::SmallVec<[CompactString; 2]>),
        http_worker: bool,
    ) -> HashSet<CaseInsensitiveString> {
        let mut links: HashSet<CaseInsensitiveString> = HashSet::new();
        let domain_name = self.domain.inner();
        let mut expanded = crate::features::glob::expand_url(&domain_name.as_str());

        if expanded.len() == 0 {
            match self.get_absolute_path(Some(domain_name)) {
                Some(u) => {
                    expanded.push(u.as_str().into());
                }
                _ => (),
            };
        };

        let blacklist_url = self.configuration.get_blacklist();

        for link in expanded {
            if self.is_allowed(&link, &blacklist_url) {
                let page = Page::new(
                    &if http_worker && link.as_ref().starts_with("https") {
                        link.inner().replacen("https", "http", 1).to_string()
                    } else {
                        link.inner().to_string()
                    },
                    &client,
                )
                .await;

                let u = page.get_url();
                let u = if u.is_empty() { link } else { u.into() };

                let link_result = match self.on_link_find_callback {
                    Some(cb) => cb(u, None),
                    _ => (u, None),
                };

                self.links_visited.insert(link_result.0);

                channel_send_page(&self.channel, page.clone());

                let page_links = HashSet::from(page.links);

                links.extend(page_links);
            }
        }

        links
    }

    /// Expand links for crawl.
    #[cfg(all(feature = "glob", not(feature = "decentralized")))]
    async fn crawl_establish(
        &mut self,
        client: &Client,
        base: &mut (CompactString, smallvec::SmallVec<[CompactString; 2]>),
        _: bool,
    ) -> HashSet<CaseInsensitiveString> {
        let mut links: HashSet<CaseInsensitiveString> = HashSet::new();
        let domain_name = self.domain.inner();
        let mut expanded = crate::features::glob::expand_url(&domain_name.as_str());

        if expanded.len() == 0 {
            match self.get_absolute_path(Some(domain_name)) {
                Some(u) => {
                    expanded.push(u.as_str().into());
                }
                _ => (),
            };
        };

        let blacklist_url = self.configuration.get_blacklist();

        for link in expanded {
            if self.is_allowed_default(&link.inner(), &blacklist_url) {
                let page = Page::new(&link.inner(), &client).await;

                match page.final_redirect_destination {
                    Some(ref domain) => {
                        let domain: Box<CaseInsensitiveString> =
                            CaseInsensitiveString::new(&domain).into();
                        self.domain_parsed = match url::Url::parse(&domain.inner()) {
                            Ok(u) => Some(Box::new(crate::page::convert_abs_path(&u, "/"))),
                            _ => None,
                        };
                        self.domain = domain;
                        match self.setup_selectors() {
                            Some(s) => {
                                base.0 = s.0;
                                base.1 = s.1;
                            }
                            _ => (),
                        }
                    }
                    _ => (),
                }

                if !page.is_empty() {
                    let u = page.get_url().into();
                    let link_result = match self.on_link_find_callback {
                        Some(cb) => cb(u, None),
                        _ => (u, None),
                    };

                    self.links_visited.insert(link_result.0);
                    let page_links = HashSet::from(page.links(&base).await);

                    links.extend(page_links);
                } else {
                    self.status = CrawlStatus::Empty;
                };

                channel_send_page(&self.channel, page);
            }
        }

        links
    }

    /// Set the crawl status depending on crawl state.
    fn set_crawl_status(&mut self) {
        if !self.domain_parsed.is_some() {
            self.status = CrawlStatus::Invalid;
        } else {
            self.status = CrawlStatus::Idle;
        }
    }

    /// Start to crawl website with async concurrency.
    pub async fn crawl(&mut self) {
        self.start();
        let (client, handle) = self.setup().await;
        let (handle, join_handle) = match handle {
            Some(h) => (Some(h.0), Some(h.1)),
            _ => (None, None),
        };
        self.crawl_concurrent(&client, &handle).await;
        self.sitemap_crawl_chain(&client, &handle, false).await;
        self.set_crawl_status();
        match join_handle {
            Some(h) => h.abort(),
            _ => (),
        };
        self.client = Some(client);
    }

    /// Start to crawl website with async concurrency using the sitemap. This does not page forward into the request. This does nothing without the `sitemap` flag enabled.
    pub async fn crawl_sitemap(&mut self) {
        self.start();
        let (client, handle) = self.setup().await;
        let (handle, join_handle) = match handle {
            Some(h) => (Some(h.0), Some(h.1)),
            _ => (None, None),
        };
        self.sitemap_crawl(&client, &handle, false).await;
        self.set_crawl_status();
        match join_handle {
            Some(h) => h.abort(),
            _ => (),
        };
        self.client = Some(client);
    }

    #[cfg(all(not(feature = "decentralized"), feature = "smart"))]
    /// Start to crawl website with async concurrency smart. Use HTTP first and JavaScript Rendering as needed. This has no effect without the `smart` flag enabled.
    pub async fn crawl_smart(&mut self) {
        self.start();
        let (client, handle) = self.setup().await;
        let (handle, join_handle) = match handle {
            Some(h) => (Some(h.0), Some(h.1)),
            _ => (None, None),
        };
        self.crawl_concurrent_smart(&client, &handle).await;
        self.set_crawl_status();
        match join_handle {
            Some(h) => h.abort(),
            _ => (),
        };
        self.client = Some(client);
    }

    #[cfg(all(not(feature = "decentralized"), not(feature = "smart")))]
    /// Start to crawl website with async concurrency smart. Use HTTP first and JavaScript Rendering as needed. This has no effect without the `smart` flag enabled.
    pub async fn crawl_smart(&mut self) {
        self.crawl().await
    }

    /// Start to crawl website with async concurrency using the base raw functionality. Useful when using the `chrome` feature and defaulting to the basic implementation.
    pub async fn crawl_raw(&mut self) {
        self.start();
        let (client, handle) = self.setup().await;
        let (handle, join_handle) = match handle {
            Some(h) => (Some(h.0), Some(h.1)),
            _ => (None, None),
        };
        self.crawl_concurrent_raw(&client, &handle).await;
        self.sitemap_crawl_chain(&client, &handle, false).await;
        self.set_crawl_status();
        match join_handle {
            Some(h) => h.abort(),
            _ => (),
        };
        self.client = Some(client);
    }

    /// Start to scrape/download website with async concurrency.
    pub async fn scrape(&mut self) {
        self.start();
        let (client, handle) = self.setup().await;
        let (handle, join_handle) = match handle {
            Some(h) => (Some(h.0), Some(h.1)),
            _ => (None, None),
        };
        self.scrape_concurrent(&client, &handle).await;
        self.sitemap_crawl_chain(&client, &handle, true).await;
        self.set_crawl_status();
        match join_handle {
            Some(h) => h.abort(),
            _ => (),
        };
        self.client = Some(client);
    }

    /// Start to crawl website with async concurrency using the base raw functionality. Useful when using the "chrome" feature and defaulting to the basic implementation.
    pub async fn scrape_raw(&mut self) {
        self.start();
        let (client, handle) = self.setup().await;
        let (handle, join_handle) = match handle {
            Some(h) => (Some(h.0), Some(h.1)),
            _ => (None, None),
        };
        self.scrape_concurrent_raw(&client, &handle).await;
        self.sitemap_crawl_chain(&client, &handle, true).await;
        self.set_crawl_status();
        match join_handle {
            Some(h) => h.abort(),
            _ => (),
        };
        self.client = Some(client);
    }

    /// Start to crawl website concurrently - used mainly for chrome instances to connect to default raw HTTP.
    async fn crawl_concurrent_raw(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        self.start();
        match self.setup_selectors() {
            Some(mut selector) => {
                let (mut interval, throttle) = self.setup_crawl();
                let blacklist_url = self.configuration.get_blacklist();
                let on_link_find_callback = self.on_link_find_callback;
                let full_resources = self.configuration.full_resources;
                let mut links: HashSet<CaseInsensitiveString> =
                    self._crawl_establish(&client, &mut selector, false).await;
                    
                let shared = Arc::new((
                    client.to_owned(),
                    selector,
                    self.channel.clone(),
                    self.configuration.external_domains_caseless.clone(),
                ));

                if !links.is_empty() {
                    let mut set: JoinSet<HashSet<CaseInsensitiveString>> = JoinSet::new();
                    let chandle = Handle::current();

                    // crawl while links exists
                    loop {
                        let stream = tokio_stream::iter::<HashSet<CaseInsensitiveString>>(
                            links.drain().collect(),
                        )
                        .throttle(*throttle);
                        tokio::pin!(stream);

                        loop {
                            match stream.next().await {
                                Some(link) => {
                                    match handle.as_ref() {
                                        Some(handle) => {
                                            while handle.load(Ordering::Relaxed) == 1 {
                                                interval.tick().await;
                                            }
                                            if handle.load(Ordering::Relaxed) == 2 || self.shutdown {
                                                set.shutdown().await;
                                                break;
                                            }
                                        }
                                        None => (),
                                    }

                                    if !self.is_allowed(&link, &blacklist_url) {
                                        continue;
                                    }

                                    log("fetch", &link);
                                    self.links_visited.insert(link.clone());
                                    let permit = SEM.acquire().await.unwrap();
                                    let shared = shared.clone();
                                    task::yield_now().await;

                                    set.spawn_on(
                                        async move {
                                            let link_result = match on_link_find_callback {
                                                Some(cb) => cb(link, None),
                                                _ => (link, None),
                                            };
                                            let mut page =
                                                Page::new_page(&link_result.0.as_ref(), &shared.0).await;
                                            page.set_external(shared.3.to_owned());

                                            let page_links = if full_resources {
                                                page.links_full(&shared.1).await
                                            } else {
                                                page.links(&shared.1).await
                                            };

                                            channel_send_page(&shared.2, page);

                                            drop(permit);

                                            page_links
                                        },
                                        &chandle,
                                    );
                                }
                                _ => break,
                            }
                        }

                        while let Some(res) = set.join_next().await {
                            match res {
                                Ok(msg) => {
                                    links.extend(&msg - &self.links_visited);
                                }
                                _ => (),
                            };
                        }

                        if links.is_empty() {
                            break;
                        }
                    }
                }
            }
            _ => log("", "The domain should be a valid URL, refer to <https://www.w3.org/TR/2011/WD-html5-20110525/urls.html#valid-url>."),
        }
    }

    /// Start to scape website concurrently and store html - used mainly for chrome instances to connect to default raw HTTP.
    async fn scrape_concurrent_raw(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        self.start();
        let selectors = get_page_selectors(
            &self.domain.inner(),
            self.configuration.subdomains,
            self.configuration.tld,
        );

        if selectors.is_some() {
            self.status = CrawlStatus::Active;
            let blacklist_url = self.configuration.get_blacklist();
            self.pages = Some(Box::new(Vec::new()));
            let delay = self.configuration.delay;
            let on_link_find_callback = self.on_link_find_callback;
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            let selectors = Arc::new(unsafe { selectors.unwrap_unchecked() });
            let full_resources = self.configuration.full_resources;
            let throttle = Duration::from_millis(delay);

            let mut links: HashSet<CaseInsensitiveString> = HashSet::from([*self.domain.clone()]);
            let mut set: JoinSet<(CaseInsensitiveString, Page, HashSet<CaseInsensitiveString>)> =
                JoinSet::new();

            // crawl while links exists
            loop {
                let stream =
                    tokio_stream::iter::<HashSet<CaseInsensitiveString>>(links.drain().collect())
                        .throttle(throttle);
                tokio::pin!(stream);

                while let Some(link) = stream.next().await {
                    match handle.as_ref() {
                        Some(handle) => {
                            while handle.load(Ordering::Relaxed) == 1 {
                                interval.tick().await;
                            }
                            if handle.load(Ordering::Relaxed) == 2 || self.shutdown {
                                set.shutdown().await;
                                break;
                            }
                        }
                        None => (),
                    }
                    if !self.is_allowed(&link, &blacklist_url) {
                        continue;
                    }
                    self.links_visited.insert(link.clone());
                    log("fetch", &link);
                    let permit = SEM.acquire().await.unwrap();
                    // these clones should move into a single arc
                    let client = client.clone();
                    let channel = self.channel.clone();
                    let selectors = selectors.clone();
                    let external_domains_caseless =
                        self.configuration.external_domains_caseless.clone();

                    set.spawn(async move {
                        drop(permit);
                        let page_resource =
                            crate::utils::fetch_page_html_raw(&link.as_ref(), &client).await;
                        let mut page = build(&link.as_ref(), page_resource);

                        let (link, _) = match on_link_find_callback {
                            Some(cb) => {
                                let c = cb(link, Some(page.get_html()));

                                c
                            }
                            _ => (link, None),
                        };

                        channel_send_page(&channel, page.clone());

                        page.set_external(external_domains_caseless);

                        let page_links = if full_resources {
                            page.links_full(&*selectors).await
                        } else {
                            page.links(&*selectors).await
                        };

                        (link, page, page_links)
                    });
                }

                task::yield_now().await;

                if links.capacity() >= 1500 {
                    links.shrink_to_fit();
                }

                while let Some(res) = set.join_next().await {
                    match res {
                        Ok(msg) => {
                            let page = msg.1;
                            links.extend(&msg.2 - &self.links_visited);
                            task::yield_now().await;
                            match self.pages.as_mut() {
                                Some(p) => p.push(page),
                                _ => (),
                            };
                        }
                        _ => (),
                    };
                }

                task::yield_now().await;
                if links.is_empty() {
                    break;
                }
            }
        }
    }

    /// Start to crawl website concurrently.
    #[cfg(all(not(feature = "decentralized"), feature = "chrome"))]
    async fn crawl_concurrent(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        self.start();
        let selectors = self.setup_selectors();

        // crawl if valid selector
        if selectors.is_some() {
            let (mut interval, throttle) = self.setup_crawl();
            let blacklist_url = self.configuration.get_blacklist();
            let on_link_find_callback = self.on_link_find_callback;
            let full_resources = self.configuration.full_resources;

            match launch_browser(&self.configuration).await {
                Some((mut browser, browser_handle)) => {
                    match browser.new_page("about:blank").await {
                        Ok(new_page) => {
                            if cfg!(feature = "chrome_stealth") || self.configuration.stealth_mode {
                                let _ = new_page.enable_stealth_mode_with_agent(&if self
                                    .configuration
                                    .user_agent
                                    .is_some()
                                {
                                    &self.configuration.user_agent.as_ref().unwrap().as_str()
                                } else {
                                    ""
                                });
                            }

                            let new_page = configure_browser(new_page, &self.configuration).await;

                            let mut selectors = unsafe { selectors.unwrap_unchecked() };

                            let chrome_page = Arc::new(new_page.clone());

                            let intercept_handle =
                                self.setup_chrome_interception(&chrome_page).await;

                            let mut links: HashSet<CaseInsensitiveString> = self
                                .crawl_establish(&client, &mut selectors, false, &chrome_page)
                                .await;

                            let shared = Arc::new((
                                client.to_owned(),
                                selectors,
                                self.channel.clone(),
                                chrome_page,
                                self.configuration.external_domains_caseless.clone(),
                            ));

                            let add_external = shared.4.len() > 0;

                            if !links.is_empty() {
                                let mut set: JoinSet<HashSet<CaseInsensitiveString>> =
                                    JoinSet::new();
                                let chandle = Handle::current();

                                // crawl while links exists
                                loop {
                                    let stream =
                                        tokio_stream::iter::<HashSet<CaseInsensitiveString>>(
                                            links.drain().collect(),
                                        )
                                        .throttle(*throttle);
                                    tokio::pin!(stream);

                                    loop {
                                        match stream.next().await {
                                            Some(link) => {
                                                match handle.as_ref() {
                                                    Some(handle) => {
                                                        while handle.load(Ordering::Relaxed) == 1 {
                                                            interval.tick().await;
                                                        }
                                                        if handle.load(Ordering::Relaxed) == 2
                                                            || self.shutdown
                                                        {
                                                            set.shutdown().await;
                                                            break;
                                                        }
                                                    }
                                                    None => (),
                                                }

                                                if !self.is_allowed(&link, &blacklist_url) {
                                                    continue;
                                                }

                                                log("fetch", &link);
                                                self.links_visited.insert(link.clone());
                                                let permit = SEM.acquire().await.unwrap();
                                                let shared = shared.clone();
                                                task::yield_now().await;

                                                set.spawn_on(
                                                    async move {
                                                        let link_result =
                                                            match on_link_find_callback {
                                                                Some(cb) => cb(link, None),
                                                                _ => (link, None),
                                                            };
                                                        let mut page = Page::new(
                                                            &link_result.0.as_ref(),
                                                            &shared.0,
                                                            &shared.3,
                                                        )
                                                        .await;

                                                        if add_external {
                                                            page.set_external(shared.4.clone());
                                                        }

                                                        let page_links = if full_resources {
                                                            page.links_full(&shared.1).await
                                                        } else {
                                                            page.links(&shared.1).await
                                                        };

                                                        channel_send_page(&shared.2, page);

                                                        drop(permit);

                                                        page_links
                                                    },
                                                    &chandle,
                                                );
                                            }
                                            _ => break,
                                        }
                                    }

                                    while let Some(res) = set.join_next().await {
                                        match res {
                                            Ok(msg) => links.extend(&msg - &self.links_visited),
                                            _ => (),
                                        };
                                    }

                                    if links.is_empty() {
                                        break;
                                    }
                                }
                            }

                            if !std::env::var("CHROME_URL").is_ok() {
                                let _ = browser.close().await;
                                let _ = browser_handle.await;
                            } else {
                                let _ = new_page.close().await;
                                if !browser_handle.is_finished() {
                                    browser_handle.abort();
                                }
                            }
                            match intercept_handle {
                                Some(intercept_handle) => {
                                    let _ = intercept_handle.await;
                                }
                                _ => (),
                            }
                        }
                        _ => log("", "Chrome failed to open page."),
                    }
                }
                _ => log("", "Chrome failed to start."),
            }
        }
    }

    /// Start to crawl website concurrently.
    #[cfg(all(not(feature = "decentralized"), not(feature = "chrome")))]
    async fn crawl_concurrent(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        self.start();
        // crawl if valid selector
        match self.setup_selectors() {
            Some(mut selector) => {
                let (mut interval, throttle) = self.setup_crawl();
                let blacklist_url = self.configuration.get_blacklist();
                let on_link_find_callback = self.on_link_find_callback;
                let full_resources = self.configuration.full_resources;
                let mut links: HashSet<CaseInsensitiveString> =
                    self.crawl_establish(&client, &mut selector, false).await;

                links.extend(self.links_visited.drain());

                let shared = Arc::new((
                    client.to_owned(),
                    selector,
                    self.channel.clone(),
                    self.configuration.external_domains_caseless.clone(),
                ));

                let add_external = shared.3.len() > 0;

                if !links.is_empty() {
                    let mut set: JoinSet<HashSet<CaseInsensitiveString>> = JoinSet::new();
                    let chandle = Handle::current();

                    // crawl while links exists
                    loop {
                        let stream = tokio_stream::iter::<HashSet<CaseInsensitiveString>>(
                            links.drain().collect(),
                        )
                        .throttle(*throttle);
                        tokio::pin!(stream);

                        loop {
                            match stream.next().await {
                                Some(link) => {
                                    match handle.as_ref() {
                                        Some(handle) => {
                                            while handle.load(Ordering::Relaxed) == 1 {
                                                interval.tick().await;
                                            }
                                            if handle.load(Ordering::Relaxed) == 2 || self.shutdown {
                                                set.shutdown().await;
                                                break;
                                            }
                                        }
                                        None => (),
                                    }

                                    if !self.is_allowed(&link, &blacklist_url) {
                                        continue;
                                    }

                                    log("fetch", &link);
                                    self.links_visited.insert(link.clone());
                                    let permit = SEM.acquire().await.unwrap();
                                    let shared = shared.clone();
                                    task::yield_now().await;

                                    set.spawn_on(
                                        async move {
                                            let link_result = match on_link_find_callback {
                                                Some(cb) => cb(link, None),
                                                _ => (link, None),
                                            };
                                            let mut page =
                                                Page::new(&link_result.0.as_ref(), &shared.0).await;

                                            if add_external {
                                                page.set_external(shared.3.to_owned());
                                            }

                                            let page_links = if full_resources {
                                                page.links_full(&shared.1).await
                                            } else {
                                                page.links(&shared.1).await
                                            };
                    
                                            channel_send_page(&shared.2, page);

                                            drop(permit);

                                            page_links
                                        },
                                        &chandle,
                                    );
                                }
                                _ => break,
                            }
                        }

                        while let Some(res) = set.join_next().await {
                            match res {
                                Ok(msg) => {
                                    links.extend(&msg - &self.links_visited);
                                }
                                _ => (),
                            };
                        }

                        if links.is_empty() {
                            break;
                        }
                    }
                }
            }
            _ => log("", "The domain should be a valid URL, refer to <https://www.w3.org/TR/2011/WD-html5-20110525/urls.html#valid-url>."),
        }
    }

    /// Start to crawl website concurrently.
    #[cfg(feature = "decentralized")]
    async fn crawl_concurrent(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        match url::Url::parse(&self.domain.inner()) {
            Ok(_) => {
                let blacklist_url = self.configuration.get_blacklist();
                let domain = self.domain.inner().as_str();
                let mut interval = Box::pin(tokio::time::interval(Duration::from_millis(10)));
                let throttle = Box::pin(self.get_delay());
                let on_link_find_callback = self.on_link_find_callback;
                // http worker verify
                let http_worker = std::env::var("SPIDER_WORKER")
                    .unwrap_or_else(|_| "http:".to_string())
                    .starts_with("http:");

                let mut links: HashSet<CaseInsensitiveString> = self
                    .crawl_establish(
                        &client,
                        &mut (domain.into(), Default::default()),
                        http_worker,
                    )
                    .await;

                links.extend(self.links_visited.drain());

                let mut set: JoinSet<HashSet<CaseInsensitiveString>> = JoinSet::new();
                let chandle = Handle::current();

                // crawl while links exists
                loop {
                    let stream = tokio_stream::iter::<HashSet<CaseInsensitiveString>>(
                        links.drain().collect(),
                    )
                    .throttle(*throttle);
                    tokio::pin!(stream);

                    loop {
                        match stream.next().await {
                            Some(link) => {
                                match handle.as_ref() {
                                    Some(handle) => {
                                        while handle.load(Ordering::Relaxed) == 1 {
                                            interval.tick().await;
                                        }
                                        if handle.load(Ordering::Relaxed) == 2 || self.shutdown {
                                            set.shutdown().await;
                                            break;
                                        }
                                    }
                                    None => (),
                                }

                                if !self.is_allowed(&link, &blacklist_url) {
                                    continue;
                                }

                                log("fetch", &link);

                                self.links_visited.insert(link.clone());
                                let permit = SEM.acquire().await.unwrap();
                                let client = client.clone();
                                task::yield_now().await;

                                set.spawn_on(
                                    async move {
                                        let link_results = match on_link_find_callback {
                                            Some(cb) => cb(link, None),
                                            _ => (link, None),
                                        };
                                        let link_results = link_results.0.as_ref();
                                        let page = Page::new(
                                            &if http_worker && link_results.starts_with("https") {
                                                link_results
                                                    .replacen("https", "http", 1)
                                                    .to_string()
                                            } else {
                                                link_results.to_string()
                                            },
                                            &client,
                                        )
                                        .await;

                                        drop(permit);

                                        page.links
                                    },
                                    &chandle,
                                );
                            }
                            _ => break,
                        }
                    }

                    while let Some(res) = set.join_next().await {
                        match res {
                            Ok(msg) => {
                                links.extend(&msg - &self.links_visited);
                            }
                            _ => (),
                        };
                    }

                    if links.is_empty() {
                        break;
                    }
                }
            }
            _ => (),
        }
    }

    /// Start to crawl website concurrently.
    #[cfg(all(not(feature = "decentralized"), feature = "smart"))]
    async fn crawl_concurrent_smart(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        self.start();
        let selectors = self.setup_selectors();

        // crawl if valid selector
        if selectors.is_some() {
            let (mut interval, throttle) = self.setup_crawl();
            let blacklist_url = self.configuration.get_blacklist();

            let on_link_find_callback = self.on_link_find_callback;

            match launch_browser(&self.configuration).await {
                Some((mut browser, browser_handle)) => {
                    match browser.new_page("about:blank").await {
                        Ok(new_page) => {
                            if cfg!(feature = "chrome_stealth") || self.configuration.stealth_mode {
                                let _ = new_page.enable_stealth_mode_with_agent(&if self
                                    .configuration
                                    .user_agent
                                    .is_some()
                                {
                                    &self.configuration.user_agent.as_ref().unwrap().as_str()
                                } else {
                                    ""
                                });
                            }

                            let mut selectors = unsafe { selectors.unwrap_unchecked() };

                            let new_page = configure_browser(new_page, &self.configuration).await;

                            let chrome_page = Arc::new(new_page.clone());

                            let intercept_handle =
                                self.setup_chrome_interception(&chrome_page).await;

                            let mut links: HashSet<CaseInsensitiveString> = self
                                .crawl_establish(&client, &mut selectors, false, &chrome_page)
                                .await;

                            links.extend(self.links_visited.drain());

                            let shared = Arc::new((
                                client.to_owned(),
                                selectors,
                                self.channel.clone(),
                                chrome_page,
                                self.configuration.external_domains_caseless.clone(),
                            ));

                            let add_external = shared.4.len() > 0;

                            if !links.is_empty() {
                                let mut set: JoinSet<HashSet<CaseInsensitiveString>> =
                                    JoinSet::new();
                                let chandle = Handle::current();

                                // crawl while links exists
                                loop {
                                    let stream =
                                        tokio_stream::iter::<HashSet<CaseInsensitiveString>>(
                                            links.drain().collect(),
                                        )
                                        .throttle(*throttle);
                                    tokio::pin!(stream);

                                    loop {
                                        match stream.next().await {
                                            Some(link) => {
                                                match handle.as_ref() {
                                                    Some(handle) => {
                                                        while handle.load(Ordering::Relaxed) == 1 {
                                                            interval.tick().await;
                                                        }
                                                        if handle.load(Ordering::Relaxed) == 2
                                                            || self.shutdown
                                                        {
                                                            set.shutdown().await;
                                                            break;
                                                        }
                                                    }
                                                    None => (),
                                                }

                                                if !self.is_allowed(&link, &blacklist_url) {
                                                    continue;
                                                }

                                                log("fetch", &link);
                                                self.links_visited.insert(link.clone());
                                                let permit = SEM.acquire().await.unwrap();
                                                let shared = shared.clone();
                                                task::yield_now().await;

                                                set.spawn_on(
                                                    async move {
                                                        let link_result =
                                                            match on_link_find_callback {
                                                                Some(cb) => cb(link, None),
                                                                _ => (link, None),
                                                            };
                                                        let mut page = Page::new(
                                                            &link_result.0.as_ref(),
                                                            &shared.0,
                                                            &shared.3,
                                                        )
                                                        .await;

                                                        if add_external {
                                                            page.set_external(shared.4.clone());
                                                        }

                                                        let page_links = page
                                                            .smart_links(&shared.1, &shared.3)
                                                            .await;

                                                        channel_send_page(&shared.2, page);

                                                        drop(permit);

                                                        page_links
                                                    },
                                                    &chandle,
                                                );
                                            }
                                            _ => break,
                                        }
                                    }

                                    while let Some(res) = set.join_next().await {
                                        match res {
                                            Ok(msg) => links.extend(&msg - &self.links_visited),
                                            _ => (),
                                        };
                                    }

                                    if links.is_empty() {
                                        break;
                                    }
                                }
                            }

                            if !std::env::var("CHROME_URL").is_ok() {
                                let _ = browser.close().await;
                                let _ = browser_handle.await;
                            } else {
                                let _ = new_page.close().await;
                                if !browser_handle.is_finished() {
                                    browser_handle.abort();
                                }
                            }

                            match intercept_handle {
                                Some(intercept_handle) => {
                                    let _ = intercept_handle.await;
                                }
                                _ => (),
                            }
                        }
                        _ => log("", "Chrome failed to open page."),
                    }
                }
                _ => log("", "Chrome failed to start."),
            }
        }
    }

    #[cfg(not(feature = "chrome"))]
    /// Start to scape website concurrently and store resources.
    async fn scrape_concurrent(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        self.start();
        let selectors = get_page_selectors(
            &self.domain.inner(),
            self.configuration.subdomains,
            self.configuration.tld,
        );

        if selectors.is_some() {
            self.status = CrawlStatus::Active;
            let blacklist_url = self.configuration.get_blacklist();
            self.pages = Some(Box::new(Vec::new()));
            let on_link_find_callback = self.on_link_find_callback;
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            let selectors = Arc::new(unsafe { selectors.unwrap_unchecked() });
            let throttle = Duration::from_millis(self.configuration.delay);
            let full_resources = self.configuration.full_resources;
            let mut links: HashSet<CaseInsensitiveString> = HashSet::from([*self.domain.clone()]);
            let mut set: JoinSet<(CaseInsensitiveString, Page, HashSet<CaseInsensitiveString>)> =
                JoinSet::new();

            links.extend(self.links_visited.drain());

            // crawl while links exists
            loop {
                let stream =
                    tokio_stream::iter::<HashSet<CaseInsensitiveString>>(links.drain().collect())
                        .throttle(throttle);
                tokio::pin!(stream);

                while let Some(link) = stream.next().await {
                    match handle.as_ref() {
                        Some(handle) => {
                            while handle.load(Ordering::Relaxed) == 1 {
                                interval.tick().await;
                            }
                            if handle.load(Ordering::Relaxed) == 2 || self.shutdown {
                                set.shutdown().await;
                                break;
                            }
                        }
                        None => (),
                    }
                    if !self.is_allowed(&link, &blacklist_url) {
                        continue;
                    }
                    self.links_visited.insert(link.clone());
                    log("fetch", &link);
                    let permit = SEM.acquire().await.unwrap();
                    // these clones should move into a single arc
                    let client = client.clone();
                    let channel = self.channel.clone();
                    let selectors = selectors.clone();
                    let external_domains_caseless =
                        self.configuration.external_domains_caseless.clone();

                    set.spawn(async move {
                        drop(permit);
                        let page_resource =
                            crate::utils::fetch_page_html(&link.as_ref(), &client).await;
                        let mut page = build(&link.as_ref(), page_resource);

                        let (link, _) = match on_link_find_callback {
                            Some(cb) => {
                                let c = cb(link, Some(page.get_html()));

                                c
                            }
                            _ => (link, None),
                        };

                        channel_send_page(&channel, page.clone());

                        page.set_external(external_domains_caseless);

                        let page_links = if full_resources {
                            page.links_full(&*selectors).await
                        } else {
                            page.links(&*selectors).await
                        };

                        (link, page, page_links)
                    });
                }

                task::yield_now().await;

                if links.capacity() >= 1500 {
                    links.shrink_to_fit();
                }

                while let Some(res) = set.join_next().await {
                    match res {
                        Ok(msg) => {
                            let page = msg.1;
                            links.extend(&msg.2 - &self.links_visited);
                            task::yield_now().await;
                            match self.pages.as_mut() {
                                Some(p) => p.push(page),
                                _ => (),
                            };
                        }
                        _ => (),
                    };
                }

                task::yield_now().await;
                if links.is_empty() {
                    break;
                }
            }
        }
    }

    #[cfg(feature = "chrome")]
    /// Start to scape website concurrently and store resources
    async fn scrape_concurrent(&mut self, client: &Client, handle: &Option<Arc<AtomicI8>>) {
        self.start();
        let selectors = get_page_selectors(
            &self.domain.inner(),
            self.configuration.subdomains,
            self.configuration.tld,
        );

        if selectors.is_some() {
            self.status = CrawlStatus::Active;
            let blacklist_url = self.configuration.get_blacklist();
            self.pages = Some(Box::new(Vec::new()));
            let delay = self.configuration.delay;
            let on_link_find_callback = self.on_link_find_callback;
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            let selectors = Arc::new(unsafe { selectors.unwrap_unchecked() });
            let throttle = Duration::from_millis(delay);

            let mut links: HashSet<CaseInsensitiveString> = HashSet::from([*self.domain.clone()]);
            let mut set: JoinSet<(CaseInsensitiveString, Page, HashSet<CaseInsensitiveString>)> =
                JoinSet::new();

            links.extend(self.links_visited.drain());

            match launch_browser(&self.configuration).await {
                Some((mut browser, _)) => {
                    match browser.new_page("about:blank").await {
                        Ok(new_page) => {
                            if cfg!(feature = "chrome_stealth") {
                                let _ = new_page.enable_stealth_mode_with_agent(&if self
                                    .configuration
                                    .user_agent
                                    .is_some()
                                {
                                    &self.configuration.user_agent.as_ref().unwrap().as_str()
                                } else {
                                    ""
                                });
                            }

                            let new_page = configure_browser(new_page, &self.configuration).await;

                            let page = Arc::new(new_page.clone());

                            let intercept_handle = self.setup_chrome_interception(&page).await;

                            // crawl while links exists
                            loop {
                                let stream = tokio_stream::iter::<HashSet<CaseInsensitiveString>>(
                                    links.drain().collect(),
                                )
                                .throttle(throttle);
                                tokio::pin!(stream);

                                while let Some(link) = stream.next().await {
                                    match handle.as_ref() {
                                        Some(handle) => {
                                            while handle.load(Ordering::Relaxed) == 1 {
                                                interval.tick().await;
                                            }
                                            if handle.load(Ordering::Relaxed) == 2 || self.shutdown
                                            {
                                                set.shutdown().await;
                                                break;
                                            }
                                        }
                                        None => (),
                                    }
                                    if !self.is_allowed(&link, &blacklist_url) {
                                        continue;
                                    }
                                    self.links_visited.insert(link.clone());
                                    log("fetch", &link);
                                    let client = client.clone();
                                    let permit = SEM.acquire().await.unwrap();
                                    let channel = self.channel.clone();
                                    let selectors = selectors.clone();
                                    let page = page.clone();
                                    let external_domains_caseless =
                                        self.configuration.external_domains_caseless.clone();

                                    set.spawn(async move {
                                        drop(permit);
                                        let page = crate::utils::fetch_page_html_chrome(
                                            &link.as_ref(),
                                            &client,
                                            &page,
                                        )
                                        .await;
                                        let mut page = build(&link.as_ref(), page);

                                        let (link, _) = match on_link_find_callback {
                                            Some(cb) => {
                                                let c = cb(link, Some(page.get_html()));

                                                c
                                            }
                                            _ => (link, None),
                                        };

                                        channel_send_page(&channel, page.clone());

                                        page.set_external(external_domains_caseless);
                                        let page_links = page.links(&*selectors).await;

                                        (link, page, page_links)
                                    });
                                }

                                task::yield_now().await;

                                if links.capacity() >= 1500 {
                                    links.shrink_to_fit();
                                }

                                while let Some(res) = set.join_next().await {
                                    match res {
                                        Ok(msg) => {
                                            let page = msg.1;
                                            links.extend(&msg.2 - &self.links_visited);
                                            task::yield_now().await;
                                            match self.pages.as_mut() {
                                                Some(p) => p.push(page),
                                                _ => (),
                                            };
                                        }
                                        _ => (),
                                    };
                                }

                                task::yield_now().await;
                                if links.is_empty() {
                                    break;
                                }
                            }

                            if !std::env::var("CHROME_URL").is_ok() {
                                let _ = browser.close().await;
                            } else {
                                let _ = new_page.close().await;
                            }

                            match intercept_handle {
                                Some(intercept_handle) => {
                                    let _ = intercept_handle.await;
                                }
                                _ => (),
                            }
                        }
                        _ => log("", "Chrome failed to open page."),
                    }
                }
                _ => log("", "Chrome failed to start."),
            };
        }
    }

    #[cfg(not(feature = "sitemap"))]
    /// Sitemap crawl entire lists. Note: this method does not re-crawl the links of the pages found on the sitemap. This does nothing without the [sitemap] flag.
    pub async fn sitemap_crawl(
        &mut self,
        _client: &Client,
        _handle: &Option<Arc<AtomicI8>>,
        _scrape: bool,
    ) {
    }

    #[cfg(not(feature = "sitemap"))]
    /// Sitemap crawl entire lists chain. Note: this method does not re-crawl the links of the pages found on the sitemap. This does nothing without the [sitemap] flag.
    async fn sitemap_crawl_chain(
        &mut self,
        _client: &Client,
        _handle: &Option<Arc<AtomicI8>>,
        _scrape: bool,
    ) {
    }

    /// Sitemap crawl entire lists. Note: this method does not re-crawl the links of the pages found on the sitemap. This does nothing without the [sitemap] flag.
    #[cfg(feature = "sitemap")]
    pub async fn sitemap_crawl_raw(
        &mut self,
        client: &Client,
        handle: &Option<Arc<AtomicI8>>,
        scrape: bool,
    ) {
        use sitemap::reader::{SiteMapEntity, SiteMapReader};
        use sitemap::structs::Location;
        let domain = self.domain.inner().as_str();
        let handle = handle.clone().unwrap_or_default();

        let mut interval = tokio::time::interval(Duration::from_millis(15));

        let (sitemap_path, needs_trailing) = match &self.configuration.sitemap_url {
            Some(sitemap_path) => {
                let sitemap_path = sitemap_path.as_str();
                if domain.ends_with('/') && sitemap_path.starts_with('/') {
                    (&sitemap_path[1..], false)
                } else if !domain.ends_with('/')
                    && !sitemap_path.is_empty()
                    && !sitemap_path.starts_with('/')
                {
                    (sitemap_path, true)
                } else {
                    (sitemap_path, false)
                }
            }
            _ => ("sitemap.xml", !domain.ends_with("/")),
        };

        self.configuration.sitemap_url = Some(Box::new(
            string_concat!(domain, if needs_trailing { "/" } else { "" }, sitemap_path).into(),
        ));

        let blacklist_url = self.configuration.get_blacklist();

        while let Some(site) = &self.configuration.sitemap_url {
            if !handle.load(Ordering::Relaxed) == 2 || self.shutdown {
                break;
            }

            let mut sitemap_added = false;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Page>(32);
            let client = client.clone();

            let channel = self.channel.clone();

            let handles = tokio::spawn(async move {
                let mut pages = Vec::new();

                while let Some(page) = rx.recv().await {
                    if channel.is_some() {
                        if scrape {
                            pages.push(page.clone());
                        };

                        channel_send_page(&channel, page);
                    } else {
                        pages.push(page);
                    }
                }

                pages
            });

            match client.get(site.as_str()).send().await {
                Ok(response) => {
                    match response.text().await {
                        Ok(text) => {
                            // <html><head><title>Invalid request</title></head><body><p>Blocked by WAF</p><
                            let mut stream =
                                tokio_stream::iter(SiteMapReader::new(text.as_bytes()));

                            while let Some(entity) = stream.next().await {
                                while handle.load(Ordering::Relaxed) == 1 {
                                    interval.tick().await;
                                }
                                // shutdown all links
                                if handle.load(Ordering::Relaxed) == 2 || self.shutdown {
                                    break;
                                }

                                match entity {
                                    SiteMapEntity::Url(url_entry) => match url_entry.loc {
                                        Location::Url(url) => {
                                            let link: CaseInsensitiveString = url.as_str().into();

                                            if !self.is_allowed(&link, &blacklist_url) {
                                                continue;
                                            }

                                            self.links_visited.insert(link.clone());

                                            let client = client.clone();
                                            let tx = tx.clone();

                                            tokio::spawn(async move {
                                                let page = Page::new(&link.inner(), &client).await;

                                                match tx.reserve().await {
                                                    Ok(permit) => {
                                                        permit.send(page);
                                                    }
                                                    _ => (),
                                                }
                                            });
                                        }
                                        Location::None | Location::ParseErr(_) => (),
                                    },
                                    SiteMapEntity::SiteMap(sitemap_entry) => {
                                        match sitemap_entry.loc {
                                            Location::Url(url) => {
                                                self.configuration
                                                    .sitemap_url
                                                    .replace(Box::new(url.as_str().into()));
                                                sitemap_added = true;
                                            }
                                            Location::None | Location::ParseErr(_) => (),
                                        }
                                    }
                                    SiteMapEntity::Err(err) => {
                                        log("incorrect sitemap error: ", err.msg())
                                    }
                                };
                            }
                        }
                        Err(err) => log("http parse error: ", err.to_string()),
                    };
                }
                Err(err) => log("http network error: ", err.to_string()),
            };

            drop(tx);

            if let Ok(handle) = handles.await {
                match self.pages.as_mut() {
                    Some(p) => p.extend(handle),
                    _ => (),
                };
            }

            if !sitemap_added {
                self.configuration.sitemap_url = None;
            };
        }
    }

    /// Sitemap crawl entire lists. Note: this method does not re-crawl the links of the pages found on the sitemap. This does nothing without the [sitemap] flag.
    #[cfg(feature = "sitemap")]
    pub async fn sitemap_crawl(
        &mut self,
        client: &Client,
        handle: &Option<Arc<AtomicI8>>,
        scrape: bool,
    ) {
        self.sitemap_crawl_raw(client, handle, scrape).await
    }

    /// Sitemap crawl entire lists chain. Note: this method does not re-crawl the links of the pages found on the sitemap. This does nothing without the [sitemap] flag.
    #[cfg(feature = "sitemap")]
    async fn sitemap_crawl_chain(
        &mut self,
        client: &Client,
        handle: &Option<Arc<AtomicI8>>,
        scrape: bool,
    ) {
        if !self.configuration.ignore_sitemap {
            self.sitemap_crawl_raw(client, handle, scrape).await
        }
    }

    /// get base link for crawl establishing
    #[cfg(feature = "regex")]
    fn get_base_link(&self) -> &CaseInsensitiveString {
        &self.domain
    }

    /// get base link for crawl establishing
    #[cfg(not(feature = "regex"))]
    fn get_base_link(&self) -> &CompactString {
        self.domain.inner()
    }

    /// Respect robots.txt file.
    pub fn with_respect_robots_txt(&mut self, respect_robots_txt: bool) -> &mut Self {
        self.configuration
            .with_respect_robots_txt(respect_robots_txt);
        self
    }

    /// Include subdomains detection.
    pub fn with_subdomains(&mut self, subdomains: bool) -> &mut Self {
        self.configuration.with_subdomains(subdomains);
        self
    }

    /// Include tld detection.
    pub fn with_tld(&mut self, tld: bool) -> &mut Self {
        self.configuration.with_tld(tld);
        self
    }

    /// Only use HTTP/2.
    pub fn with_http2_prior_knowledge(&mut self, http2_prior_knowledge: bool) -> &mut Self {
        self.configuration
            .with_http2_prior_knowledge(http2_prior_knowledge);
        self
    }

    /// Delay between request as ms.
    pub fn with_delay(&mut self, delay: u64) -> &mut Self {
        self.configuration.with_delay(delay);
        self
    }

    /// Max time to wait for request.
    pub fn with_request_timeout(&mut self, request_timeout: Option<Duration>) -> &mut Self {
        self.configuration.with_request_timeout(request_timeout);
        self
    }

    /// Add user agent to request.
    pub fn with_user_agent(&mut self, user_agent: Option<&str>) -> &mut Self {
        self.configuration.with_user_agent(user_agent);
        self
    }

    #[cfg(feature = "sitemap")]
    /// Add user agent to request. This does nothing without the [sitemap] flag enabled.
    pub fn with_sitemap(&mut self, sitemap_url: Option<&str>) -> &mut Self {
        self.configuration.with_sitemap(sitemap_url);
        self
    }

    #[cfg(not(feature = "sitemap"))]
    /// Add user agent to request. This does nothing without the [sitemap] flag enabled.
    pub fn with_sitemap(&mut self, _sitemap_url: Option<&str>) -> &mut Self {
        self
    }

    /// Use proxies for request.
    pub fn with_proxies(&mut self, proxies: Option<Vec<String>>) -> &mut Self {
        self.configuration.with_proxies(proxies);
        self
    }

    /// Set a crawl ID to use for tracking crawls. This does nothing without the [control] flag enabled.
    #[cfg(not(feature = "control"))]
    pub fn with_crawl_id(&mut self, _crawl_id: String) -> &mut Self {
        self
    }

    /// Set a crawl ID to use for tracking crawls. This does nothing without the [control] flag enabled.
    #[cfg(feature = "control")]
    pub fn with_crawl_id(&mut self, crawl_id: String) -> &mut Self {
        self.crawl_id = crawl_id.into();
        self
    }

    /// Add blacklist urls to ignore.
    pub fn with_blacklist_url<T>(&mut self, blacklist_url: Option<Vec<T>>) -> &mut Self
    where
        Vec<CompactString>: From<Vec<T>>,
    {
        self.configuration.with_blacklist_url(blacklist_url);
        self
    }

    /// Set HTTP headers for request using [reqwest::header::HeaderMap](https://docs.rs/reqwest/latest/reqwest/header/struct.HeaderMap.html).
    pub fn with_headers(&mut self, headers: Option<reqwest::header::HeaderMap>) -> &mut Self {
        self.configuration.with_headers(headers);
        self
    }

    #[cfg(feature = "budget")]
    /// Set a crawl budget per path with levels support /a/b/c or for all paths with "*". This does nothing without the `budget` flag enabled.
    pub fn with_budget(&mut self, budget: Option<HashMap<&str, u32>>) -> &mut Self {
        self.configuration.with_budget(budget);
        self
    }

    #[cfg(not(feature = "budget"))]
    /// Set a crawl budget per path with levels support /a/b/c or for all paths with "*". This does nothing without the `budget` flag enabled.
    pub fn with_budget(&mut self, budget: Option<HashMap<&str, u32>>) -> &mut Self {
        self.configuration.with_budget(budget);
        self
    }

    #[cfg(feature = "budget")]
    /// Set the crawl budget directly. This does nothing without the `budget` flag enabled.
    pub fn set_crawl_budget(&mut self, budget: Option<HashMap<CaseInsensitiveString, u32>>) {
        self.configuration.budget = budget;
    }

    #[cfg(not(feature = "budget"))]
    /// Set the crawl budget directly. This does nothing without the `budget` flag enabled.
    pub fn set_crawl_budget(&mut self, _budget: Option<HashMap<CaseInsensitiveString, u32>>) {}

    /// Set a crawl depth limit. If the value is 0 there is no limit. This does nothing without the feat flag `budget` enabled.
    pub fn with_depth(&mut self, depth: usize) -> &mut Self {
        self.configuration.with_depth(depth);
        self
    }

    /// Group external domains to treat the crawl as one. If None is passed this will clear all prior domains.
    pub fn with_external_domains<'a, 'b>(
        &mut self,
        external_domains: Option<impl Iterator<Item = String> + 'a>,
    ) -> &mut Self {
        self.configuration.with_external_domains(external_domains);
        self
    }

    /// Perform a callback to run on each link find.
    pub fn with_on_link_find_callback(
        &mut self,
        on_link_find_callback: Option<
            fn(CaseInsensitiveString, Option<String>) -> (CaseInsensitiveString, Option<String>),
        >,
    ) -> &mut Self {
        match on_link_find_callback {
            Some(callback) => self.on_link_find_callback = Some(callback.into()),
            _ => self.on_link_find_callback = None,
        };
        self
    }

    /// Cookie string to use in request. This does nothing without the [cookies] flag enabled.
    pub fn with_cookies(&mut self, cookie_str: &str) -> &mut Self {
        self.configuration.with_cookies(cookie_str);
        self
    }

    /// Setup cron jobs to run. This does nothing without the [cron] flag enabled.
    pub fn with_cron(&mut self, cron_str: &str, cron_type: CronType) -> &mut Self {
        self.configuration.with_cron(cron_str, cron_type);
        self
    }

    /// Overrides default host system locale with the specified one. This does nothing without the `chrome` flag enabled.
    pub fn with_locale(&mut self, locale: Option<String>) -> &mut Self {
        self.configuration.with_locale(locale);
        self
    }

    /// Use stealth mode for the request. This does nothing without the `chrome` flag enabled.
    pub fn with_stealth(&mut self, stealth_mode: bool) -> &mut Self {
        self.configuration.with_stealth(stealth_mode);
        self
    }

    /// Cache the page following HTTP rules. This method does nothing if the [cache] feature is not enabled.
    pub fn with_caching(&mut self, cache: bool) -> &mut Self {
        self.configuration.with_caching(cache);
        self
    }

    /// Configures the viewport of the browser, which defaults to 800x600. This method does nothing if the [chrome] feature is not enabled.
    pub fn with_viewport(&mut self, viewport: Option<crate::configuration::Viewport>) -> &mut Self {
        self.configuration.with_viewport(viewport);
        self
    }

    /// Set the max redirects allowed for request.
    pub fn with_redirect_limit(&mut self, redirect_limit: usize) -> &mut Self {
        self.configuration.with_redirect_limit(redirect_limit);
        self
    }

    /// Set the redirect policy to use, either Strict or Loose by default.
    pub fn with_redirect_policy(&mut self, policy: RedirectPolicy) -> &mut Self {
        self.configuration.with_redirect_policy(policy);
        self
    }

    #[cfg(feature = "chrome_intercept")]
    /// Use request intercept for the request to only allow content that matches the host. If the content is from a 3rd party it needs to be part of our include list. This method does nothing if the `chrome_intercept` flag is not enabled.
    pub fn with_chrome_intercept(
        &mut self,
        chrome_intercept: bool,
        block_images: bool,
    ) -> &mut Self {
        self.configuration
            .with_chrome_intercept(chrome_intercept, block_images);
        self
    }

    #[cfg(not(feature = "chrome_intercept"))]
    /// Use request intercept for the request to only allow content required for the page that matches the host. If the content is from a 3rd party it needs to be part of our include list. This method does nothing if the `chrome_intercept` flag is not enabled.
    pub fn with_chrome_intercept(
        &mut self,
        chrome_intercept: bool,
        block_images: bool,
    ) -> &mut Self {
        self.configuration
            .with_chrome_intercept(chrome_intercept, block_images);
        self
    }

    /// Determine whether to collect all the resources found on pages.
    pub fn with_full_resources(&mut self, full_resources: bool) -> &mut Self {
        self.configuration.with_full_resources(full_resources);
        self
    }

    #[cfg(feature = "sitemap")]
    /// Ignore the sitemap when crawling. This method does nothing if the `sitemap` flag is not enabled.
    pub fn with_ignore_sitemap(&mut self, ignore_sitemap: bool) -> &mut Self {
        self.configuration.with_ignore_sitemap(ignore_sitemap);
        self
    }

    #[cfg(not(feature = "sitemap"))]
    /// Ignore the sitemap when crawling. This method does nothing if the `sitemap` flag is not enabled.
    pub fn with_ignore_sitemap(&mut self, ignore_sitemap: bool) -> &mut Self {
        self.configuration.with_ignore_sitemap(ignore_sitemap);
        self
    }

    /// Overrides default host system timezone with the specified one. This does nothing without the `chrome` flag enabled.
    pub fn with_timezone_id(&mut self, timezone_id: Option<String>) -> &mut Self {
        self.configuration.with_timezone_id(timezone_id);
        self
    }

    /// Set the configuration for the website directly.
    pub fn with_config(&mut self, config: Configuration) -> &mut Self {
        self.configuration = config.into();
        self
    }

    /// Set a crawl page limit. If the value is 0 there is no limit. This does nothing without the feat flag `budget` enabled.
    pub fn with_limit(&mut self, limit: u32) -> &mut Self {
        self.configuration.with_limit(limit);
        self
    }

    /// Build the website configuration when using with_builder.
    #[cfg(not(feature = "napi"))]
    pub fn build(&self) -> Result<Self, std::io::Error> {
        if self.domain_parsed.is_none() {
            Err(std::io::ErrorKind::NotFound.into())
        } else {
            Ok(self.to_owned())
        }
    }

    /// Build the website configuration when using with_builder with napi error handling.
    #[cfg(feature = "napi")]
    pub fn build(&self) -> Result<Self, WebsiteBuilderError> {
        if self.domain_parsed.is_none() {
            Err(napi::Error::new(
                WebsiteBuilderError::ValidationError("domain cannot parse"),
                "incorrect domain name",
            ))
        } else {
            Ok(self.to_owned())
        }
    }

    #[cfg(feature = "budget")]
    /// Determine if the budget has a wildcard path and the depth limit distance. This does nothing without the `budget` flag enabled.
    fn determine_limits(&mut self) {
        if self.configuration.budget.is_some() {
            let wild_card_budget = match &self.configuration.budget {
                Some(budget) => budget.contains_key(&*WILD_CARD_PATH),
                _ => false,
            };
            self.configuration.wild_card_budgeting = wild_card_budget;
        }
        if self.configuration.depth > 0 && self.domain_parsed.is_some() {
            match &self.domain_parsed {
                Some(domain) => match domain.path_segments() {
                    Some(segments) => {
                        let segments_cnt = segments.count();

                        if segments_cnt > self.configuration.depth {
                            self.configuration.depth_distance = self.configuration.depth
                                + self.configuration.depth.abs_diff(segments_cnt);
                        } else {
                            self.configuration.depth_distance = self.configuration.depth;
                        }
                    }
                    _ => (),
                },
                _ => (),
            }
        }
    }

    #[cfg(not(feature = "budget"))]
    /// Determine if the budget has a wildcard path and the depth limit distance. This does nothing without the `budget` flag enabled.
    fn determine_limits(&mut self) {}

    /// Setup subscription for data.
    #[cfg(not(feature = "sync"))]
    pub fn subscribe(
        &mut self,
        capacity: usize,
    ) -> Option<Arc<(broadcast::Sender<Page>, broadcast::Receiver<Page>)>> {
        None
    }

    /// Setup subscription for data. This will panic if capacity is equal to 0 or larger than usize::MAX / 2. This does nothing without the `sync` flag enabled.
    #[cfg(feature = "sync")]
    pub fn subscribe(&mut self, capacity: usize) -> Option<broadcast::Receiver<Page>> {
        let channel = self.channel.get_or_insert_with(|| {
            let (tx, rx) = broadcast::channel(capacity);

            (tx, Arc::new(rx))
        });

        let rx2 = channel.0.subscribe();

        Some(rx2)
    }

    /// Remove subscriptions for data. This is useful for auto droping subscriptions that are running on another thread. This does nothing without the `sync` flag enabled.
    #[cfg(not(feature = "sync"))]
    pub fn unsubscribe(&mut self) {}

    /// Remove subscriptions for data. This is useful for auto droping subscriptions that are running on another thread. This does nothing without the `sync` flag enabled.
    #[cfg(feature = "sync")]
    pub fn unsubscribe(&mut self) {
        self.channel.take();
    }

    #[cfg(feature = "cron")]
    /// Start a cron job - if you use subscribe on another thread you need to abort the handle in conjuction with runner.stop.
    pub async fn run_cron(&self) -> Runner {
        async_job::Runner::new()
            .add(Box::new(self.clone()))
            .run()
            .await
    }
}

/// Channel broadcast send the Page to receivers.
fn channel_send_page(
    channel: &Option<(
        tokio::sync::broadcast::Sender<Page>,
        std::sync::Arc<tokio::sync::broadcast::Receiver<Page>>,
    )>,
    page: Page,
) {
    match channel {
        Some(c) => match c.0.send(page) {
            _ => (),
        },
        _ => (),
    };
}

#[cfg(feature = "cron")]
/// Start a cron job taking ownership of the website
pub async fn run_cron(website: Website) -> Runner {
    async_job::Runner::new().add(Box::new(website)).run().await
}

#[cfg(feature = "cron")]
#[async_trait]
impl Job for Website {
    fn schedule(&self) -> Option<async_job::Schedule> {
        match self.configuration.cron_str.parse() {
            Ok(schedule) => Some(schedule),
            Err(e) => {
                log::error!("{:?}", e);
                None
            }
        }
    }
    async fn handle(&mut self) {
        log::info!(
            "CRON: {} - cron job running {}",
            self.get_domain().as_ref(),
            self.now()
        );
        if self.configuration.cron_type == CronType::Crawl {
            self.crawl().await;
        } else {
            self.scrape().await;
        }
    }
}

/// builder pattern error handling
#[cfg(feature = "napi")]
#[derive(Debug)]
pub enum WebsiteBuilderError {
    /// Uninitialized field
    UninitializedField(&'static str),
    /// Custom validation error
    ValidationError(&'static str),
}

#[cfg(feature = "napi")]
impl AsRef<str> for WebsiteBuilderError {
    fn as_ref(&self) -> &str {
        match self {
            Self::UninitializedField(s) => s,
            Self::ValidationError(s) => s,
        }
    }
}

#[cfg(feature = "napi")]
impl std::fmt::Display for Website {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "`{}`", self)
    }
}

#[cfg(feature = "napi")]
impl std::error::Error for Website {}

#[cfg(not(feature = "decentralized"))]
#[tokio::test]
async fn crawl() {
    let url = "https://choosealicense.com";
    let mut website: Website = Website::new(&url);
    website.crawl().await;
    assert!(
        website
            .links_visited
            .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
        "{:?}",
        website.links_visited
    );
}

#[cfg(feature = "cron")]
#[tokio::test]
async fn crawl_cron() {
    let url = "https://choosealicense.com";
    let mut website: Website = Website::new(&url)
        .with_cron("1/5 * * * * *", Default::default())
        .build()
        .unwrap();
    let mut rx2 = website.subscribe(16).unwrap();

    // handle an event on every cron
    let join_handle = tokio::spawn(async move {
        let mut links_visited = HashSet::new();
        while let Ok(res) = rx2.recv().await {
            let url = res.get_url();
            links_visited.insert(CaseInsensitiveString::new(url));
        }
        assert!(
            links_visited
                .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
            "{:?}",
            links_visited
        );
    });

    let mut runner = website.run_cron().await;
    log::debug!("Starting the Runner for 10 seconds");
    tokio::time::sleep(Duration::from_secs(10)).await;
    runner.stop().await;
    join_handle.abort();
    let _ = join_handle.await;
}

#[cfg(feature = "cron")]
#[tokio::test]
async fn crawl_cron_own() {
    let url = "https://choosealicense.com";
    let mut website: Website = Website::new(&url)
        .with_cron("1/5 * * * * *", Default::default())
        .build()
        .unwrap();
    let mut rx2 = website.subscribe(16).unwrap();

    // handle an event on every cron
    let join_handle = tokio::spawn(async move {
        let mut links_visited = HashSet::new();
        while let Ok(res) = rx2.recv().await {
            let url = res.get_url();
            links_visited.insert(CaseInsensitiveString::new(url));
        }
        assert!(
            links_visited
                .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
            "{:?}",
            links_visited
        );
    });

    let mut runner = run_cron(website).await;
    log::debug!("Starting the Runner for 10 seconds");
    tokio::time::sleep(Duration::from_secs(10)).await;
    let _ = tokio::join!(runner.stop(), join_handle);
}

#[cfg(not(feature = "decentralized"))]
#[tokio::test]
async fn scrape() {
    let mut website: Website = Website::new("https://choosealicense.com");
    website.scrape().await;
    assert!(
        website
            .links_visited
            .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
        "{:?}",
        website.links_visited
    );

    assert_eq!(website.get_pages().unwrap()[0].get_html().is_empty(), false);
}

#[tokio::test]
#[cfg(not(feature = "decentralized"))]
async fn crawl_invalid() {
    let mut website: Website = Website::new("https://w.com");
    website.crawl().await;
    assert_eq!(website.links_visited.len() <= 1, true); // only the target url should exist
}

#[tokio::test]
#[cfg(feature = "decentralized")]
async fn crawl_invalid() {
    let domain = "https://w.com";
    let mut website: Website = Website::new(domain);
    website.crawl().await;
    let mut uniq: Box<HashSet<CaseInsensitiveString>> = Box::new(HashSet::new());
    uniq.insert(format!("{}/", domain.to_string()).into()); // TODO: remove trailing slash mutate

    assert_eq!(website.links_visited, uniq); // only the target url should exist
}

#[tokio::test]
async fn not_crawl_blacklist() {
    let mut website: Website = Website::new("https://choosealicense.com");
    website.configuration.blacklist_url = Some(Box::new(Vec::from([CompactString::from(
        "https://choosealicense.com/licenses/",
    )])));

    website.crawl().await;
    assert!(
        !website
            .links_visited
            .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
        "{:?}",
        website.links_visited
    );
}

#[tokio::test]
#[cfg(feature = "regex")]
async fn not_crawl_blacklist_regex() {
    let mut website: Website = Website::new("https://choosealicense.com");
    website.with_blacklist_url(Some(Vec::from(["choosealicense.com".into()])));
    website.crawl().await;
    assert_eq!(website.links_visited.len(), 0);
}

#[test]
#[cfg(feature = "ua_generator")]
fn randomize_website_agent() {
    assert_eq!(get_ua().is_empty(), false);
}

#[tokio::test]
#[cfg(not(feature = "decentralized"))]
async fn test_respect_robots_txt() {
    let mut website: Website = Website::new("https://stackoverflow.com");
    website.configuration.respect_robots_txt = true;
    website.configuration.user_agent = Some(Box::new("*".into()));

    let (client, _): (Client, Option<(Arc<AtomicI8>, tokio::task::JoinHandle<()>)>) =
        website.setup().await;

    website.configure_robots_parser(client).await;

    assert_eq!(website.configuration.delay, 0);

    assert!(!&website.is_allowed(
        &"https://stackoverflow.com/posts/".into(),
        &Default::default()
    ));

    // test match for bing bot
    let mut website_second: Website = Website::new("https://www.mongodb.com");
    website_second.configuration.respect_robots_txt = true;
    website_second.configuration.user_agent = Some(Box::new("bingbot".into()));

    let (client_second, _): (Client, Option<(Arc<AtomicI8>, tokio::task::JoinHandle<()>)>) =
        website_second.setup().await;
    website_second.configure_robots_parser(client_second).await;

    assert_eq!(website_second.configuration.delay, 60000); // should equal one minute in ms

    // test crawl delay with wildcard agent [DOES not work when using set agent]
    let mut website_third: Website = Website::new("https://www.mongodb.com");
    website_third.configuration.respect_robots_txt = true;
    let (client_third, _): (Client, Option<(Arc<AtomicI8>, tokio::task::JoinHandle<()>)>) =
        website_third.setup().await;

    website_third.configure_robots_parser(client_third).await;

    assert_eq!(website_third.configuration.delay, 10000); // should equal 10 seconds in ms
}

#[cfg(not(feature = "decentralized"))]
#[tokio::test]
async fn test_crawl_subdomains() {
    let mut website: Website = Website::new("https://choosealicense.com");
    website.configuration.subdomains = true;
    website.crawl().await;
    assert!(
        website
            .links_visited
            .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
        "{:?}",
        website.links_visited
    );
}

#[tokio::test]
async fn test_with_configuration() {
    let mut website = Website::new("https://choosealicense.com");

    website
        .with_respect_robots_txt(true)
        .with_subdomains(true)
        .with_tld(false)
        .with_delay(0)
        .with_request_timeout(None)
        .with_http2_prior_knowledge(false)
        .with_ignore_sitemap(true)
        .with_user_agent(Some("myapp/version".into()))
        .with_headers(None)
        .with_proxies(None);

    website.crawl().await;

    assert!(
        website.links_visited.len() >= 1,
        "{:?}",
        website.links_visited
    );
}

#[cfg(all(feature = "glob", not(feature = "decentralized")))]
#[tokio::test]
async fn test_crawl_glob() {
    let mut website: Website =
        Website::new("https://choosealicense.com/licenses/{mit,apache-2.0,mpl-2.0}/");
    website.crawl().await;

    // check for either https/http in collection
    assert!(
        website
            .links_visited
            .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into())
            || website
                .links_visited
                .contains::<CaseInsensitiveString>(&"http://choosealicense.com/licenses/".into()),
        "{:?}",
        website.links_visited
    );
}

#[cfg(not(feature = "decentralized"))]
#[tokio::test]
async fn test_crawl_tld() {
    let mut website: Website = Website::new("https://choosealicense.com");
    website.configuration.tld = true;
    website.crawl().await;

    assert!(
        website
            .links_visited
            .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
        "{:?}",
        website.links_visited
    );
}

#[tokio::test]
#[cfg(all(feature = "sync", not(feature = "decentralized")))]
async fn test_crawl_subscription() {
    let mut website: Website = Website::new("https://choosealicense.com");
    let mut rx2 = website.subscribe(100).unwrap();
    let count = Arc::new(tokio::sync::Mutex::new(0));
    let count1 = count.clone();

    tokio::spawn(async move {
        while let Ok(_) = rx2.recv().await {
            let mut lock = count1.lock().await;
            *lock += 1;
        }
    });

    website.crawl().await;
    let website_links = website.get_links().len();
    let count = *count.lock().await;

    // no subscription if did not fulfill. The root page is always captured in links.
    assert!(count == website_links, "{:?}", true);
}

#[cfg(all(feature = "socks", not(feature = "decentralized")))]
#[tokio::test]
async fn test_crawl_proxy() {
    let mut website: Website = Website::new("https://choosealicense.com");
    website
        .configuration
        .proxies
        .get_or_insert(Default::default())
        .push("socks5://127.0.0.1:1080".into());

    website.crawl().await;

    let mut license_found = false;

    for links_visited in website.links_visited.iter() {
        // Proxy may return http or https in socks5 per platform.
        // We may want to replace the protocol with the host of the platform regardless of proxy response.
        if links_visited.as_ref().contains("/licenses/") {
            license_found = true;
        };
    }

    assert!(license_found, "{:?}", website.links_visited);
}

#[tokio::test]
async fn test_link_duplicates() {
    fn has_unique_elements<T>(iter: T) -> bool
    where
        T: IntoIterator,
        T::Item: Eq + std::hash::Hash,
    {
        let mut uniq = HashSet::new();
        iter.into_iter().all(move |x| uniq.insert(x))
    }

    let mut website: Website = Website::new("http://0.0.0.0:8000");
    website.crawl().await;

    assert!(has_unique_elements(&*website.links_visited));
}

#[tokio::test]
#[cfg(feature = "budget")]
async fn test_crawl_budget() {
    let mut website: Website = Website::new("https://choosealicense.com");
    website.with_budget(Some(HashMap::from([("*", 1), ("/licenses", 1)])));
    website.crawl().await;

    assert!(website.links_visited.len() <= 1);
}

#[tokio::test]
#[cfg(feature = "control")]
#[ignore]
async fn test_crawl_pause_resume() {
    use crate::utils::{pause, resume};

    let domain = "https://choosealicense.com/";
    let mut website: Website = Website::new(&domain);

    let start = tokio::time::Instant::now();

    tokio::spawn(async move {
        pause(domain).await;
        // static website test pause/resume - scan will never take longer than 5secs for target website choosealicense
        tokio::time::sleep(Duration::from_millis(5000)).await;
        resume(domain).await;
    });

    website.crawl().await;

    let duration = start.elapsed();

    assert!(duration.as_secs() >= 5, "{:?}", duration);

    assert!(
        website
            .links_visited
            .contains::<CaseInsensitiveString>(&"https://choosealicense.com/licenses/".into()),
        "{:?}",
        website.links_visited
    );
}

#[cfg(feature = "control")]
#[tokio::test]
#[ignore]
async fn test_crawl_shutdown() {
    use crate::utils::shutdown;

    // use target blog to prevent shutdown of prior crawler
    let domain = "https://rsseau.fr/";
    let mut website: Website = Website::new(&domain);

    tokio::spawn(async move {
        shutdown(domain).await;
    });

    website.crawl().await;

    assert_eq!(website.links_visited.len(), 1);
}

#[tokio::test]
#[cfg(all(feature = "cache", not(feature = "decentralized")))]
async fn test_cache() {
    let domain = "https://choosealicense.com/";
    let mut website: Website = Website::new(&domain);
    website.configuration.cache = true;

    let fresh_start = tokio::time::Instant::now();
    website.crawl().await;
    let fresh_duration = fresh_start.elapsed();

    let cached_start = tokio::time::Instant::now();
    website.crawl().await;
    let cached_duration = cached_start.elapsed();

    // cache should be faster at least 5x.
    assert!(
        fresh_duration.as_millis() > cached_duration.as_millis() * 5,
        "{:?}",
        cached_duration
    );
}
