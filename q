        thread_names: &[&str],
        resolve_gateway: impl Fn(&str) -> Result<String, BridgeError> + 'static,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        let specs = specs_for_names(thread_names);
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir(
            &specs,
            resolve_gateway,
            cache_dir,
        )
    }

    fn new_with_thread_specs_and_gateway_resolver_and_cache_dir(
        thread_specs: &[ThreadSpec],
        resolve_gateway: impl Fn(&str) -> Result<String, BridgeError> + 'static,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self, BridgeError> {
        Self::new_with_thread_specs_and_gateway_resolver_and_cache_dir_and_initial_cwd(
            thread_specs,
            resolve_gateway,
            cache_dir,
            None,
            None,
            None,
        )
    }

    fn new_with_thread_specs_and_gateway_resolver_and_cache_dir_and_initial_cwd(
        thread_specs: &[ThreadSpec],
        resolve_gateway: impl Fn(&str) -> Result<String, BridgeError> + 'static,
        cache_dir: Option<PathBuf>,
        initial_cwd: Option<PathBuf>,
        initial_project_path: Option<PathBuf>,
        panel_state: Option<Arc<PanelStateStore>>,
    ) -> Result<Self, BridgeError> {
        // Boxed immediately so the same resolver this constructor uses to
        // seed `gateway_urls` up front can also be kept on the struct for
        // later lazy provisioning (`ensure_gateway_provisioned`) -- one
        // resolver, one code path, whether a provider is known now or
        // only requested later.
        let resolve_gateway: Box<dyn Fn(&str) -> Result<String, BridgeError>> =
            Box::new(resolve_gateway);
        let default_provider = thread_specs.first().map(|spec| spec.provider.clone());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(BridgeError::Runtime)?;

        let store = match &cache_dir {
            Some(dir) => Some(JsonlStore::open(dir.clone()).map_err(BridgeError::Cache)?),
            None => None,
        };
        let events: Arc<Mutex<VecDeque<BridgeEvent>>> = Arc::new(Mutex::new(VecDeque::new()));
        let mut slots = Vec::with_capacity(thread_specs.len());

        // Resolve (and, for the production resolver, auto-spawn if
        // needed) every distinct provider's gateway once, up front --
        // not inside the per-thread loop below, so two threads sharing a
        // provider (the normal case: v1's four static threads alternate
        // codex/claude, two threads per provider) never race each other
