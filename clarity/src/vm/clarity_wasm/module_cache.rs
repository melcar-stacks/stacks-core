//! A process wide cache of loaded Wasm modules.
//!
//! Loading a Wasm module (parsing, validating and translating it to wasmi bytecode) costs more than
//! executing most contract calls, so modules are loaded once and reused by every call to the same
//! contract, across transactions.
//!
//! A [`Module`] can only be instantiated with the [`Engine`] it was loaded with, so the cache owns
//! the engine used by every [`GlobalContext`](crate::vm::contexts::GlobalContext). wasmi never
//! frees the code of a module before its engine is dropped, so the cache starts over with a new
//! engine once the code loaded with the current one exceeds [`ENGINE_BUDGET_BYTES`]. The previous
//! engine is dropped once the transactions still holding it complete.
//!
//! The cache also keeps a [`Linker`] defining the host functions for its engine, so that they are
//! not defined again for every call.
//!
//! The engine translates modules eagerly, when they are loaded: with lazy translation, wasmi
//! charges the fuel of translating a function to the first call executing it, which would make the
//! fuel consumed by a call depend on whether its module was already cached.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use wasmi::{CompilationMode, Config, Engine, Linker, Module};

use super::{ClarityWasmContext, link_host_functions};
use crate::vm::errors::VmExecutionError;
use crate::vm::types::QualifiedContractIdentifier;

/// A linker defining the host functions.
pub type HostLinker = Linker<ClarityWasmContext<'static, 'static>>;

/// The size of the Wasm code loaded with an engine after which the cache starts over with a new
/// engine, freeing the code loaded with the previous one.
const ENGINE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

struct CachedModule {
    /// The Wasm code the module was loaded from.
    wasm: Vec<u8>,
    module: Module,
}

struct ModuleCache {
    engine: Engine,
    modules: HashMap<QualifiedContractIdentifier, CachedModule>,
    /// The size of the Wasm code loaded with `engine`, cached or not.
    loaded_bytes: usize,
    /// The host functions, defined for `engine` on first use.
    host_linker: Option<HostLinker>,
}

impl ModuleCache {
    fn new() -> Self {
        let mut config = Config::default();
        config.consume_fuel(true);
        config.compilation_mode(CompilationMode::Eager);
        Self {
            engine: Engine::new(&config),
            modules: HashMap::new(),
            loaded_bytes: 0,
            host_linker: None,
        }
    }

    /// Accounts for `wasm` being loaded with the current engine, and starts over with a new engine
    /// if the budget is exceeded.
    fn account(&mut self, wasm: &[u8]) {
        self.loaded_bytes = self.loaded_bytes.saturating_add(wasm.len());
        if self.loaded_bytes > ENGINE_BUDGET_BYTES {
            *self = Self::new();
        }
    }
}

static CACHE: LazyLock<Mutex<ModuleCache>> = LazyLock::new(|| Mutex::new(ModuleCache::new()));

fn cache() -> MutexGuard<'static, ModuleCache> {
    // The cache is always left consistent, so a panic while it was held does not invalidate it.
    CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The engine to execute new transactions with.
pub fn engine() -> Engine {
    cache().engine.clone()
}

/// Loads the module of `contract`, from the cache when the same code was loaded before with
/// `engine`.
pub fn load_contract_module(
    engine: &Engine,
    contract: &QualifiedContractIdentifier,
    wasm: &[u8],
) -> Result<Module, wasmi::Error> {
    let mut cache = cache();
    if !Engine::same(engine, &cache.engine) {
        // The cache started over since `engine` was handed out.
        drop(cache);
        return Module::new(engine, wasm);
    }

    if let Some(cached) = cache
        .modules
        .get(contract)
        .filter(|cached| cached.wasm == wasm)
    {
        return Ok(cached.module.clone());
    }

    let module = Module::new(engine, wasm)?;
    cache.modules.insert(
        contract.clone(),
        CachedModule {
            wasm: wasm.to_vec(),
            module: module.clone(),
        },
    );
    cache.account(wasm);
    Ok(module)
}

/// Loads a module which is not cached, such as the module of a contract being deployed.
pub fn load_module(engine: &Engine, wasm: &[u8]) -> Result<Module, wasmi::Error> {
    let module = Module::new(engine, wasm)?;
    let mut cache = cache();
    if Engine::same(engine, &cache.engine) {
        cache.account(wasm);
    }
    Ok(module)
}

fn new_host_linker(engine: &Engine) -> Result<HostLinker, VmExecutionError> {
    let mut linker = Linker::new(engine);
    link_host_functions(&mut linker)?;
    Ok(linker)
}

/// A linker defining the host functions for `engine`, to which the definitions specific to a call
/// can be added.
pub fn host_linker(engine: &Engine) -> Result<HostLinker, VmExecutionError> {
    let mut cache = cache();
    if !Engine::same(engine, &cache.engine) {
        drop(cache);
        return new_host_linker(engine);
    }
    let linker = match &cache.host_linker {
        Some(linker) => linker.clone(),
        None => {
            let linker = new_host_linker(engine)?;
            cache.host_linker = Some(linker.clone());
            linker
        }
    };
    Ok(linker)
}
