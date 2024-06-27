use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::Context as _;
use colored::Colorize;
use wasmtime::{
    component::{Component, Func, Instance, Linker, ResourceTable, Val},
    Config, Engine, Store,
};
use wasmtime_wasi::{
    HostOutputStream, Stdout, StdoutStream, StreamResult, Subscribe, WasiCtx, WasiCtxBuilder,
    WasiView,
};

use crate::{
    command::parser::{self, ItemIdent},
    wit::WorldResolver,
};

pub struct Runtime {
    engine: Engine,
    pub store: Store<Context>,
    instance: Instance,
    linker: Linker<Context>,
    component: (Component, Vec<u8>),
    import_impls: ImportImpls,
}

impl Runtime {
    pub fn init(
        component_bytes: Vec<u8>,
        resolver: &WorldResolver,
        stub_import: impl Fn(&str) + Sync + Send + Clone + 'static,
    ) -> anyhow::Result<Self> {
        let engine = load_engine()?;
        let component = load_component(&engine, &component_bytes)?;
        let mut linker = Linker::<Context>::new(&engine);
        linker.allow_shadowing(true);

        let imports_wasi_cli = resolver.imports_wasi_cli();
        if imports_wasi_cli {
            log::debug!("Linking with wasi");
            wasmtime_wasi::add_to_linker_sync(&mut linker)?;
        }
        for (import_name, import) in resolver.imports(!imports_wasi_cli) {
            let import_name = resolver.world_item_name(import_name);
            let stub_import = stub_import.clone();
            match import {
                wit_parser::WorldItem::Function(f) => {
                    linker.root().func_new(&f.name, move |_ctx, _args, _rets| {
                        stub_import(&import_name);
                        Ok(())
                    })?;
                }
                wit_parser::WorldItem::Interface { id, .. } => {
                    let interface = resolver.interface_by_id(*id).unwrap();
                    let mut root = linker.root();
                    let mut instance = root.instance(&import_name)?;
                    for (_, f) in interface.functions.iter() {
                        let stub_import = stub_import.clone();
                        let import_name = import_name.clone();
                        instance.func_new(&f.name, move |_ctx, _args, _rets| {
                            stub_import(&import_name);
                            Ok(())
                        })?;
                    }
                    for (name, t) in &interface.types {
                        let t = resolver.type_by_id(*t).unwrap();
                        if let wit_parser::TypeDefKind::Resource = &t.kind {
                            let ty = wasmtime::component::ResourceType::host::<()>();
                            instance.resource(name, ty, |_, _| Ok(())).unwrap();
                        }
                    }
                }
                _ => {}
            }
        }
        let pre = linker
            .instantiate_pre(&component)
            .context("could not instantiate component")?;
        let mut store = build_store(&engine);
        let instance = pre.instantiate(&mut store)?;
        let import_impls = ImportImpls::new(&engine, String::from("import"));
        Ok(Self {
            engine,
            store,
            instance,
            linker,
            component: (component, component_bytes),
            import_impls,
        })
    }

    pub fn get_func(&mut self, ident: ItemIdent) -> anyhow::Result<Func> {
        let func = match ident.interface {
            Some(i) => {
                let mut exports = self.instance.exports(&mut self.store);
                let instance_name = i.to_string();
                exports
                    .instance(&instance_name)
                    .with_context(|| {
                        format!("could not find exported instance with name '{instance_name}'")
                    })?
                    .func(ident.item)
            }
            None => self
                .instance
                .exports(&mut self.store)
                .root()
                .func(ident.item),
        };
        func.with_context(|| format!("could not find function '{ident}' in instance"))
    }

    pub fn call_func(
        &mut self,
        func: Func,
        args: &[Val],
        result_count: usize,
    ) -> anyhow::Result<Vec<Val>> {
        let mut results = vec![Val::Bool(Default::default()); result_count];
        func.call(&mut self.store, args, &mut results)?;
        func.post_return(&mut self.store)?;
        Ok(results)
    }

    /// Stub a function with an export from the component encoded in `component_bytes`
    ///
    /// This function does not check that the component in `components_bytes` has the
    /// export needed.
    pub fn stub(
        &mut self,
        resolver: &WorldResolver,
        import_ident: parser::Ident<'_>,
        export_ident: parser::Ident<'_>,
        component_bytes: &[u8],
    ) -> anyhow::Result<()> {
        match (import_ident, export_ident) {
            (parser::Ident::Item(import_ident), parser::Ident::Item(export_ident)) => {
                self.stub_function(resolver, import_ident, export_ident, component_bytes)
            }
            (parser::Ident::Interface(import_ident), parser::Ident::Interface(export_ident)) => {
                self.stub_interface(resolver, import_ident, export_ident, component_bytes)
            }
            (parser::Ident::Interface(_), parser::Ident::Item(_)) => {
                anyhow::bail!("cannot satisfy interface import with a function")
            }
            (parser::Ident::Item(_), parser::Ident::Interface(_)) => {
                anyhow::bail!("cannot satisfy function import with an interface")
            }
        }
    }

    pub fn stub_interface(
        &mut self,
        resolver: &WorldResolver,
        import_ident: parser::InterfaceIdent<'_>,
        export_ident: parser::InterfaceIdent<'_>,
        component_bytes: &[u8],
    ) -> anyhow::Result<()> {
        let component = load_component(&self.engine, component_bytes)?;
        let mut linker = Linker::<ImportImplsContext>::new(&self.engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;
        let mut root = self.linker.root();
        let mut import_instance = root
            .instance(&import_ident.to_string())
            .with_context(|| format!("no imported instance named '{import_ident}' found"))?;
        let import = resolver
            .imported_interface(import_ident)
            .with_context(|| format!("no imported interface named '{import_ident}' found"))?;
        let other = WorldResolver::from_bytes(component_bytes)?;
        let export = other
            .exported_interface(export_ident)
            .with_context(|| format!("no exported interface named '{export_ident}' found"))?;
        {
            let mut store_lock = self.import_impls.store.lock().unwrap();
            let export_instance = linker.instantiate(&mut *store_lock, &component)?;
            for (fun_name, imported_function) in &import.functions {
                let exported_function = export
                    .functions
                    .get(fun_name)
                    .with_context(|| format!("no exported function named '{fun_name}' found"))?;
                if imported_function.params.len() != exported_function.params.len() {
                    anyhow::bail!("different number of parameters")
                }
                for ((arg_name, p1), (_, p2)) in imported_function
                    .params
                    .iter()
                    .zip(&exported_function.params)
                {
                    if !types_equal(resolver, p1, &other, p2) {
                        anyhow::bail!(
                            "different types for arg '{arg_name}' in function '{fun_name}'"
                        )
                    }
                }
                match (&imported_function.results, &exported_function.results) {
                    (wit_parser::Results::Named(is), wit_parser::Results::Named(es)) => {
                        if is.len() != es.len() {
                            anyhow::bail!("different number of return types")
                        }
                        let es = es
                            .iter()
                            .map(|(name, ty)| (name, ty))
                            .collect::<HashMap<&String, &wit_parser::Type>>();
                        for (name, ty) in is {
                            let e = es.get(name).with_context(|| format!("exported function '{fun_name}' does not have return value '{name}'"))?;
                            if !types_equal(resolver, ty, &other, e) {
                                anyhow::bail!("return value '{name}' has differing types");
                            }
                        }
                    }
                    (wit_parser::Results::Anon(t1), wit_parser::Results::Anon(t2)) => {
                        if !types_equal(resolver, t1, &other, t2) {
                            anyhow::bail!("return types did not match for function {fun_name}");
                        }
                    }
                    _ => anyhow::bail!("different return type kinds for function '{fun_name}'"),
                }
                let store = self.import_impls.store.clone();

                let export_func = {
                    let mut exports = export_instance.exports(&mut *store_lock);
                    let mut export_instance = exports
                        .instance(&export_ident.to_string())
                        .with_context(|| {
                            format!("no exported instance named '{export_ident} found'")
                        })?;
                    export_instance
                        .func(fun_name)
                        .with_context(|| format!("no exported function named '{fun_name}' found"))?
                };
                import_instance.func_new(fun_name, move |_ctx, args, results| {
                    let mut store = store.lock().unwrap();
                    export_func.call(&mut *store, args, results)?;
                    export_func.post_return(&mut *store)?;
                    Ok(())
                })?;
            }
        }
        self.refresh()?;
        Ok(())
    }

    pub fn stub_function(
        &mut self,
        resolver: &WorldResolver,
        import_ident: parser::ItemIdent<'_>,
        export_ident: parser::ItemIdent<'_>,
        component_bytes: &[u8],
    ) -> anyhow::Result<()> {
        // type checking
        let import = resolver
            .imported_function(import_ident)
            .with_context(|| format!("no import with name '{import_ident}'"))?;
        let other = WorldResolver::from_bytes(component_bytes)?;
        let export = other
            .exported_function(export_ident)
            .with_context(|| format!("no export with name '{export_ident}'"))?;
        if import.params != export.params {
            anyhow::bail!("params not equal")
        }
        if import.results != export.results {
            anyhow::bail!("return values not equal")
        }

        let component = load_component(&self.engine, component_bytes)?;
        let mut linker = Linker::<ImportImplsContext>::new(&self.engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;
        let export_func = {
            let mut store_lock = self.import_impls.store.lock().unwrap();
            let export_instance = linker.instantiate(&mut *store_lock, &component)?;
            match export_ident.interface {
                Some(interface) => {
                    let mut export = export_instance.exports(&mut *store_lock);
                    let mut instance = export
                        .instance(&interface.to_string())
                        .with_context(|| format!("no export named '{interface} found'"))?;
                    instance.func(export_ident.item)
                }
                None => export_instance.get_func(&mut *store_lock, export_ident.item),
            }
        }
        .with_context(|| format!("no function found named '{export_ident}'"))?;

        let store = self.import_impls.store.clone();
        let name = import_ident.item.to_owned();
        match import_ident.interface {
            Some(interface) => {
                let mut instance = self
                    .linker
                    .instance(&interface.to_string())
                    .with_context(|| format!("no interface named '{interface}' found"))?;
                instance.func_new(&name, move |_ctx, args, results| {
                    let mut store = store.lock().unwrap();
                    export_func.call(&mut *store, args, results)?;
                    export_func.post_return(&mut *store)?;
                    Ok(())
                })?;
            }
            None => {
                self.linker
                    .root()
                    .func_new(&name, move |_ctx, args, results| {
                        let mut store = store.lock().unwrap();
                        export_func.call(&mut *store, args, results)?;
                        export_func.post_return(&mut *store)?;
                        Ok(())
                    })?;
            }
        }
        self.refresh()?;
        Ok(())
    }

    pub fn set_component(&mut self, component: Vec<u8>) -> anyhow::Result<()> {
        self.component = (Component::from_binary(&self.engine, &component)?, component);
        self.refresh()
    }

    pub fn compose(&mut self, adapter: &[u8]) -> Result<(), anyhow::Error> {
        let temp = std::env::temp_dir();
        let tmp_virt = temp.join("virt.wasm");
        std::fs::write(&tmp_virt, adapter)?;
        let tmp_component = temp.join("component.wasm");
        std::fs::write(&tmp_component, &self.component.1)?;

        let bytes = wasm_compose::composer::ComponentComposer::new(
            &tmp_component,
            &wasm_compose::config::Config {
                definitions: vec![tmp_virt],
                ..Default::default()
            },
        )
        .compose()?;
        self.set_component(bytes)
    }

    pub fn component_bytes(&self) -> &[u8] {
        &self.component.1
    }

    /// Get a new instance
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        self.store = build_store(&self.engine);
        self.instance = self
            .linker
            .instantiate(&mut self.store, &self.component.0)?;
        Ok(())
    }
}

/// A collection of instances that implement the main components imports
struct ImportImpls {
    store: Arc<Mutex<Store<ImportImplsContext>>>,
}

impl ImportImpls {
    fn new(engine: &Engine, prefix: String) -> Self {
        let table = ResourceTable::new();
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stderr();
        builder.stdout(ImportImplStdout::new(prefix));
        let wasi = builder.build();
        let context = ImportImplsContext::new(table, wasi);
        let store = Store::new(engine, context);

        Self {
            store: Arc::new(Mutex::new(store)),
        }
    }
}

struct ImportImplStdout {
    stream: Box<dyn HostOutputStream>,
    prefix: String,
}

impl ImportImplStdout {
    fn new(prefix: String) -> Self {
        let prefix = format!("<{}>", prefix).green().bold();
        let stream = Stdout.stream();
        Self {
            stream,
            prefix: prefix.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl HostOutputStream for ImportImplStdout {
    fn write(&mut self, bytes: bytes::Bytes) -> StreamResult<()> {
        let output = String::from_utf8_lossy(&bytes);
        let output = format!("{} {output}", self.prefix);
        self.stream.write(output.into_bytes().into())
    }

    fn flush(&mut self) -> StreamResult<()> {
        self.stream.flush()
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        self.stream.check_write()
    }

    async fn write_ready(&mut self) -> StreamResult<usize> {
        self.stream.write_ready().await
    }
}

#[async_trait::async_trait]
impl Subscribe for ImportImplStdout {
    async fn ready(&mut self) {
        self.stream.ready().await
    }
}

#[async_trait::async_trait]
impl StdoutStream for ImportImplStdout {
    fn stream(&self) -> Box<(dyn HostOutputStream + 'static)> {
        Stdout.stream()
    }

    fn isatty(&self) -> bool {
        Stdout.isatty()
    }
}

fn build_store(engine: &Engine) -> Store<Context> {
    let table = ResourceTable::new();
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdout().inherit_stderr();
    let wasi = builder.build();
    let context = Context::new(table, wasi);
    Store::new(engine, context)
}

pub struct Context {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl Context {
    fn new(table: ResourceTable, wasi: WasiCtx) -> Self {
        Self { table, wasi }
    }
}

impl WasiView for Context {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

fn load_engine() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);

    Engine::new(&config)
}

fn load_component(engine: &Engine, component_bytes: &[u8]) -> anyhow::Result<Component> {
    Component::new(engine, component_bytes)
}

struct ImportImplsContext {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl ImportImplsContext {
    fn new(table: ResourceTable, wasi: WasiCtx) -> Self {
        Self { table, wasi }
    }
}

impl WasiView for ImportImplsContext {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

fn types_equal(
    resolver1: &WorldResolver,
    t1: &wit_parser::Type,
    resolver2: &WorldResolver,
    t2: &wit_parser::Type,
) -> bool {
    match (t1, t2) {
        (wit_parser::Type::Id(t1), wit_parser::Type::Id(t2)) => {
            let t1 = resolver1.type_by_id(*t1).unwrap();
            let t2 = resolver2.type_by_id(*t2).unwrap();
            type_defs_equal(resolver1, &t1.kind, resolver2, &t2.kind)
        }
        (wit_parser::Type::Id(t1), t2) => {
            let t1 = resolver1.type_by_id(*t1).unwrap();
            if let wit_parser::TypeDefKind::Type(t1) = &t1.kind {
                types_equal(resolver1, t1, resolver2, t2)
            } else {
                false
            }
        }
        (t1, wit_parser::Type::Id(t2)) => {
            let t2 = resolver1.type_by_id(*t2).unwrap();
            if let wit_parser::TypeDefKind::Type(t2) = &t2.kind {
                types_equal(resolver1, t1, resolver2, t2)
            } else {
                false
            }
        }
        (t1, t2) => t1 == t2,
    }
}

fn type_defs_equal(
    resolver1: &WorldResolver,
    t1: &wit_parser::TypeDefKind,
    resolver2: &WorldResolver,
    t2: &wit_parser::TypeDefKind,
) -> bool {
    match (t1, t2) {
        (wit_parser::TypeDefKind::Result(r1), wit_parser::TypeDefKind::Result(r2)) => {
            let oks = match (&r1.ok, &r2.ok) {
                (None, None) => true,
                (Some(t1), Some(t2)) => types_equal(resolver1, t1, resolver2, t2),
                _ => false,
            };
            let errs = match (&r1.err, &r2.err) {
                (None, None) => true,
                (Some(t1), Some(t2)) => types_equal(resolver1, t1, resolver2, t2),
                _ => false,
            };
            oks && errs
        }
        (wit_parser::TypeDefKind::List(t1), wit_parser::TypeDefKind::List(t2)) => {
            types_equal(resolver1, t1, resolver2, t2)
        }
        (wit_parser::TypeDefKind::Variant(v1), wit_parser::TypeDefKind::Variant(v2)) => {
            if v1.cases.len() != v2.cases.len() {
                return false;
            }
            v1.cases.iter().zip(v2.cases.iter()).all(|(c1, c2)| {
                let types_equal = match (&c1.ty, &c2.ty) {
                    (Some(t1), Some(t2)) => types_equal(resolver1, t1, resolver2, t2),
                    (None, None) => true,
                    _ => false,
                };
                c1.name == c2.name && types_equal
            })
        }
        // TODO: more comparisons
        _ => false,
    }
}
